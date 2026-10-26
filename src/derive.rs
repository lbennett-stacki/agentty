use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::jobs::{FanItem, FanKind, Job, JobState, Tempo};
use crate::model::{
    AgentId, At, AutoGroup, ConfigDir, Cost, Count, Ctx, Detail, DiagSource, Diagnostic,
    JobOutcome, Limits, Meta, Need, Node, NodeKind, NodeState, OpenTarget, PaneInfo, Parked,
    Problem, Row, RowKey, SafeText, Snapshot, Status, TaskId, TaskOutcome, Tree, UserKey,
    WaitingFor, Window,
};
use crate::registry::{Name, NameSource, ProcKind, RegEntry, RegStatus};
use crate::sidecars::{Statusline, Task, TaskStatus, Team};
use crate::subagents::{AgentMeta, Subagents};
use crate::transcript::{BgKind, Facts, Turn};

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
    pub panes: &'a [PaneInfo],
}

#[derive(Clone, Copy)]
pub struct ExitedView<'a> {
    pub dir: &'a ConfigDir,
    pub job: &'a Job,
    pub facts: Option<&'a Facts>,
}

pub type AgentView<'a> = (&'a AgentMeta, NodeState, Option<&'a Facts>);

const DETAIL_FRESH_MS: i64 = 2 * 60 * 1000;

pub fn snapshot<'a>(
    sessions: impl Iterator<Item = SessionView<'a>>,
    exited: impl Iterator<Item = ExitedView<'a>>,
    cached_limits: impl Iterator<Item = (&'a ConfigDir, Limits)>,
    world_diagnostics: &[Diagnostic],
    now: At,
) -> Snapshot {
    let mut snap = Snapshot {
        rows: Vec::new(),
        diagnostics: world_diagnostics.to_vec(),
        limits: cached_limits.map(|(d, u)| (d.clone(), u)).collect(),
    };
    for v in sessions {
        if let Some(seen) = v.statusline.and_then(|s| s.limits) {
            snap.limits
                .entry(v.dir.clone())
                .and_modify(|u| *u = u.merge(seen))
                .or_insert(seen);
        }
        if v.reg.hidden {
            continue;
        }
        unknown_values(v.dir, Some(v.reg), v.job, &mut snap.diagnostics);
        snap.rows.push(live_row(v, now));
    }
    for v in exited {
        unknown_values(v.dir, None, Some(v.job), &mut snap.diagnostics);
        snap.rows.push(exited_row(v));
    }
    snap
}

/// The account `n` should start on: one with no full window, then the most 7-day room, then the
/// most 5-hour room. When every account is full, the one that frees up first. An account with no
/// observation counts as empty. Ties keep the configured order.
pub fn roomiest(accounts: &[ConfigDir], limits: &BTreeMap<ConfigDir, Limits>, now: At) -> usize {
    let key = |dir: &ConfigDir| {
        let u = limits.get(dir);
        let windows = || {
            u.into_iter()
                .flat_map(|u| [u.five_hour, u.seven_day])
                .flatten()
        };
        let free_at = windows()
            .filter(|w| w.used_now(now) >= 100.0)
            .map(|w| w.resets_at)
            .max();
        let used = |w: Option<Window>| w.map_or(0.0, |w| w.used_now(now));
        (
            free_at,
            used(u.and_then(|u| u.seven_day)),
            used(u.and_then(|u| u.five_hour)),
        )
    };
    let keys: Vec<_> = accounts.iter().map(key).collect();
    (0..keys.len())
        .min_by(|&a, &b| {
            let (fa, sa, ha) = keys[a];
            let (fb, sb, hb) = keys[b];
            // `None` (nothing full) must sort before any reset time.
            (fa.is_some(), fa)
                .cmp(&(fb.is_some(), fb))
                .then(sa.total_cmp(&sb))
                .then(ha.total_cmp(&hb))
        })
        .unwrap_or(0)
}

fn unknown_values(
    dir: &ConfigDir,
    reg: Option<&RegEntry>,
    job: Option<&Job>,
    out: &mut Vec<Diagnostic>,
) {
    let mut push = |source, field, value: &str, path: String| {
        out.push(Diagnostic {
            source,
            problem: Problem::UnknownValue {
                field,
                value: value.to_owned(),
            },
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
    let key = RowKey::Live {
        dir: v.dir.clone(),
        pid: v.reg.pid,
        proc_start: v.reg.proc_start.clone(),
    };
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
        detail: detail(&v, &agents, now),
        open: open_target(&v),
        title,
        status,
    }
}

pub fn exited_row(v: ExitedView<'_>) -> Row {
    let outcome = job_outcome(v.job);
    let title = pick_title(v.facts, None, Some(v.job), &v.job.id.0);
    let now = v
        .job
        .detail
        .clone()
        .unwrap_or_else(|| SafeText::ours(outcome_word(outcome)));
    let facts = v.facts;
    Row {
        key: RowKey::Exited {
            dir: v.dir.clone(),
            job: v.job.id.clone(),
        },
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
            last_text: facts
                .and_then(|f| f.last_text.as_ref())
                .map(|t| t.first_line.clone()),
            open_tasks: Vec::new(),
            pending: Vec::new(),
            cwd: v.job.worktree_path.clone().unwrap_or_default(),
            config_dir: v.dir.0.clone(),
            session_id: v.job.session_id.clone(),
            job_id: Some(v.job.id.clone()),
        },
        open: OpenTarget::Attach {
            dir: v.dir.clone(),
            job: v.job.id.clone(),
        },
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

pub fn status(v: &SessionView<'_>, agents: &[AgentView<'_>], now: At) -> (Status, At) {
    let reg_since = v.reg.status_since;
    match &v.reg.status {
        RegStatus::Waiting(w) => (Status::NeedsYou(need_of(w, v.facts)), reg_since),
        RegStatus::Busy => {
            let Some(facts) = v.facts else {
                return (Status::Working, reg_since);
            };
            match facts.turn {
                Turn::Open { since } => (Status::Working, reg_since.max(since)),
                // The registry turns busy as the prompt is submitted; the transcript shows the
                // opener a tick later. Busy dedupes on [status, waitingFor], so a status newer
                // than the close can only be a new turn, never the parked state continuing.
                Turn::Closed { at, .. } | Turn::Interrupted { at } if reg_since > at => {
                    (Status::Working, reg_since)
                }
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
                return (
                    Status::NeedsYou(Need::JobQuestion(needs.clone())),
                    reg_since,
                );
            }
            let zero = Count::Exact(0);
            if let Some(p) = Parked::new(zero, zero, zero, 0, future_wakeup(v.facts, now)) {
                return (Status::Parked(p), reg_since);
            }
            let asked_question = v.facts.is_some_and(|f| {
                matches!(f.turn, Turn::Closed { .. })
                    && f.last_text.as_ref().is_some_and(|t| t.is_question)
            });
            (Status::Idle { asked_question }, reg_since)
        }
        RegStatus::Other(_) => (
            Status::Idle {
                asked_question: false,
            },
            reg_since,
        ),
    }
}

fn future_wakeup(facts: Option<&Facts>, now: At) -> Option<At> {
    facts.and_then(|f| f.wakeup).filter(|w| *w > now)
}

pub fn need_of(w: &WaitingFor, facts: Option<&Facts>) -> Need {
    match w {
        WaitingFor::Permission => Need::Permission {
            tool: facts
                .and_then(|f| f.last_tool.as_ref())
                .filter(|t| !t.answered)
                .map(|t| t.name.clone()),
        },
        other => Need::Waiting(other.clone()),
    }
}

pub fn parked(
    v: &SessionView<'_>,
    agents: &[AgentView<'_>],
    registry_says_shell: bool,
    now: At,
) -> Parked {
    let (mut n_agents, mut n_shells, n_monitors, mut n_workflows) = match v.job {
        Some(job) => {
            let count = |k: FanKind| job.open_work().filter(|f| f.kind == k).count() as u16;
            (
                Count::Exact(count(FanKind::Agent)),
                Count::Exact(count(FanKind::Shell)),
                Count::Exact(count(FanKind::Monitor)),
                count(FanKind::Workflow),
            )
        }
        None => {
            let complete = v.facts.is_some_and(|f| f.horizon.from_start);
            let count = |n: u16, exact: bool| {
                if exact {
                    Count::Exact(n)
                } else {
                    Count::AtLeast(n)
                }
            };
            let side = sidecar_work(v, agents);
            let fold = |k| v.facts.map_or(0, |f| f.open_count(k)) + side.count(k);
            let pending = match v.facts.map(|f| f.turn) {
                Some(Turn::Closed { pending_agents, .. }) => pending_agents,
                _ => 0,
            };
            let running = agents
                .iter()
                .filter(|(_, s, _)| *s == NodeState::Running)
                .count() as u16;
            let agents_n = v.facts.map_or(0, |f| f.open_count(BgKind::Agent));
            let agents_n = agents_n.max(running).max(pending);
            (
                count(agents_n, complete || v.subagents.is_some()),
                count(fold(BgKind::Shell), complete && side.complete),
                count(fold(BgKind::Monitor), complete && side.complete),
                0,
            )
        }
    };
    if registry_says_shell {
        n_agents = Count::Exact(0);
        n_workflows = 0;
        if !n_shells.is_positive() && !n_monitors.is_positive() {
            n_shells = Count::AtLeast(1);
        }
    } else if !n_agents.is_positive() && n_workflows == 0 {
        n_agents = Count::AtLeast(1);
    }
    Parked::new(
        n_agents,
        n_shells,
        n_monitors,
        n_workflows,
        future_wakeup(v.facts, now),
    )
    .expect("the registry floors above guarantee a positive count")
}

pub fn title(v: &SessionView<'_>) -> SafeText {
    let short: String = v.reg.session_id.0.chars().take(8).collect();
    pick_title(v.facts, v.reg.name.as_ref(), v.job, &short)
}

fn pick_title(
    facts: Option<&Facts>,
    reg_name: Option<&Name>,
    job: Option<&Job>,
    fallback: &str,
) -> SafeText {
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

pub fn now_line(status: &Status, v: &SessionView<'_>, now: At) -> SafeText {
    let fresh_detail = v
        .job
        .filter(|j| {
            j.updated_at
                .is_some_and(|u| now.millis_since(u) <= DETAIL_FRESH_MS)
        })
        .and_then(|j| j.detail.clone());
    let empty = SafeText::default;
    match status {
        Status::NeedsYou(Need::Permission { .. }) => {
            let label = WaitingFor::Permission.label();
            match v
                .facts
                .and_then(|f| f.last_tool.as_ref())
                .filter(|t| !t.answered)
            {
                Some(call) => SafeText::redact(&format!(
                    "{label} · {}",
                    tool_line(&call.name, &call.summary).as_str()
                )),
                None => SafeText::ours(label),
            }
        }
        Status::NeedsYou(Need::Waiting(w)) => SafeText::redact(w.label()),
        Status::NeedsYou(Need::JobQuestion(q)) => q.clone(),
        Status::Working => fresh_detail
            .or_else(|| v.facts.and_then(doing))
            .unwrap_or_else(empty),
        Status::Parked(p) => SafeText::redact(&format!("waiting on {}", describe_parked(p))),
        Status::Idle { .. } => fresh_detail
            .or_else(|| {
                let f = v.facts?;
                f.away_summary
                    .clone()
                    .or_else(|| f.last_text.as_ref().map(|t| t.first_line.clone()))
                    .or_else(|| f.last_prompt.clone())
            })
            .unwrap_or_else(empty),
        Status::Exited(o) => v
            .job
            .and_then(|j| j.detail.clone())
            .unwrap_or_else(|| SafeText::ours(outcome_word(*o))),
    }
}

fn doing(f: &Facts) -> Option<SafeText> {
    let tool = f
        .last_tool
        .as_ref()
        .map(|t| (t.at, tool_line(&t.name, &t.summary)));
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

pub fn tree(v: &SessionView<'_>, agents: &[AgentView<'_>], now: At) -> Tree {
    let known: HashSet<&AgentId> = agents.iter().map(|(m, _, _)| &m.id).collect();
    let mut t = agent_level(v, agents, &known, None, 0);
    t.live.extend(leaves(v, roster(v), &known));
    if let Some(at) = future_wakeup(v.facts, now) {
        t.live.push(Node {
            id: TaskId("wakeup".to_owned()),
            kind: NodeKind::Wakeup,
            state: NodeState::Running,
            label: SafeText::redact(&format!("wakes {}", clock(at))),
            since: v
                .facts
                .and_then(|f| f.last_activity)
                .unwrap_or(v.reg.status_since),
            now: None,
            children: Tree::default(),
        });
    }
    t
}

/// Leaf nodes for open work that has no meta file of its own: shells, monitors, workflows,
/// and agents the tree does not already place from their meta.
fn leaves<'a>(
    v: &SessionView<'_>,
    items: impl IntoIterator<Item = RosterItem<'a>>,
    known: &HashSet<&AgentId>,
) -> Vec<Node> {
    items
        .into_iter()
        .filter(|item| item.kind != Work::Agent || !known.contains(&AgentId(item.id.0.clone())))
        .map(|item| Node {
            id: item.id,
            kind: match item.kind {
                Work::Agent => NodeKind::Agent {
                    agent_type: SafeText::ours("agent"),
                    model: None,
                },
                Work::Shell => NodeKind::Shell,
                Work::Monitor => NodeKind::Monitor,
                Work::Workflow => NodeKind::Workflow,
            },
            state: NodeState::Running,
            label: item.label.clone(),
            since: item.since.unwrap_or(v.reg.status_since),
            now: None,
            children: Tree::default(),
        })
        .collect()
}

// Real trees are at most two agents deep. The bound also stops a malformed self-parent cycle.
const MAX_TREE_DEPTH: u8 = 2;

fn agent_level(
    v: &SessionView<'_>,
    agents: &[AgentView<'_>],
    known: &HashSet<&AgentId>,
    parent: Option<&AgentId>,
    depth: u8,
) -> Tree {
    let mut t = Tree::default();
    if depth > MAX_TREE_DEPTH {
        return t;
    }
    for &(meta, state, sidecar) in agents {
        let placed_under = meta.parent.as_ref().filter(|p| known.contains(p));
        if placed_under != parent {
            continue;
        }
        let state = fan_agent(v.job, &meta.id).map_or(state, fan_state);
        if state != NodeState::Running {
            t.finished += 1;
            continue;
        }
        let mut children = agent_level(v, agents, known, Some(&meta.id), depth + 1);
        if v.job.is_none()
            && let Some(side) = sidecar
        {
            children.live.extend(leaves(v, open_items(side), known));
        }
        t.live.push(Node {
            id: TaskId::from(&meta.id),
            kind: NodeKind::Agent {
                agent_type: meta.agent_type.clone(),
                model: meta.model.clone(),
            },
            state,
            label: meta.description.clone(),
            since: agent_started(v, agents, &meta.id, sidecar),
            now: sidecar.and_then(doing),
            children,
        });
    }
    t
}

fn fan_agent<'a>(job: Option<&'a Job>, id: &AgentId) -> Option<&'a FanItem> {
    job?.fan
        .iter()
        .find(|f| f.kind == FanKind::Agent && f.id == id.0)
}

fn fan_state(f: &FanItem) -> NodeState {
    match (f.done_at, f.failed) {
        (None, _) => NodeState::Running,
        (Some(_), true) => NodeState::Finished(TaskOutcome::Failed),
        (Some(_), false) => NodeState::Finished(TaskOutcome::Completed),
    }
}

fn agent_started(
    v: &SessionView<'_>,
    agents: &[AgentView<'_>],
    id: &AgentId,
    own: Option<&Facts>,
) -> At {
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
                let id = TaskId(f.id.clone());
                let described = v
                    .facts
                    .and_then(|facts| facts.open_bg.get(&id))
                    .map(|b| &b.label);
                Some(RosterItem {
                    label: described.unwrap_or(&f.label),
                    id,
                    kind,
                    since: f.started_at,
                })
            })
            .collect();
    }
    v.facts.into_iter().flat_map(open_items).collect()
}

fn open_items(f: &Facts) -> impl Iterator<Item = RosterItem<'_>> {
    f.open_bg.iter().map(|(id, b)| RosterItem {
        id: id.clone(),
        kind: match b.kind {
            BgKind::Agent => Work::Agent,
            BgKind::Shell => Work::Shell,
            BgKind::Monitor => Work::Monitor,
        },
        label: &b.label,
        since: Some(b.started),
    })
}

/// Work launched inside running subagents. Background shells and monitors started by an agent
/// are recorded only in its sidecar (677 of the launches on this machine), never in the parent
/// transcript, while the registry's `shell` status counts them all the same.
struct SidecarWork<'a> {
    items: Vec<RosterItem<'a>>,
    /// False when a running agent's sidecar was not read (deferred, or budgeted out) or its
    /// window missed the file's start, so a count over `items` is a lower bound.
    complete: bool,
}

impl SidecarWork<'_> {
    fn count(&self, kind: BgKind) -> u16 {
        let want = match kind {
            BgKind::Agent => Work::Agent,
            BgKind::Shell => Work::Shell,
            BgKind::Monitor => Work::Monitor,
        };
        self.items.iter().filter(|i| i.kind == want).count() as u16
    }
}

fn sidecar_work<'a>(v: &SessionView<'_>, agents: &[AgentView<'a>]) -> SidecarWork<'a> {
    let mut work = SidecarWork {
        items: Vec::new(),
        complete: true,
    };
    if v.job.is_some() {
        return work;
    }
    for (_, _, side) in agents.iter().filter(|(_, s, _)| *s == NodeState::Running) {
        match side {
            Some(f) => {
                work.complete &= f.horizon.from_start;
                work.items
                    .extend(open_items(f).filter(|i| i.kind != Work::Agent));
            }
            None => work.complete = false,
        }
    }
    work
}

pub fn meta(v: &SessionView<'_>) -> Meta {
    let usage = v.facts.and_then(|f| f.usage.as_ref());
    let statusline = v.statusline.filter(|s| usage.is_none_or(|u| s.at >= u.at));
    let percent = statusline.and_then(|s| {
        let window = s.window.filter(|w| *w > 0)?;
        let pct = s
            .ctx_used_pct
            .or_else(|| usage.map(|u| u.ctx_tokens as f64 * 100.0 / window as f64))?;
        Some(Ctx::Percent(pct.round().clamp(0.0, 100.0) as u8))
    });
    let cost = statusline
        .and_then(|s| s.cost_usd)
        .map(Cost::Live)
        .or_else(|| {
            v.facts
                .and_then(|f| f.exit)
                .map(|e| Cost::Final(e.cost_usd))
        });
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

fn branch(facts: Option<&Facts>, job: Option<&Job>) -> Option<SafeText> {
    // `gitBranch` is the literal "HEAD" on a detached checkout, which names nothing.
    let named = |b: &&SafeText| b.as_str() != "HEAD";
    let live = facts.and_then(|f| f.branch.as_ref()).filter(named);
    let worktree = facts
        .and_then(|f| f.worktree.as_ref())
        .and_then(|w| w.branch.as_ref());
    let job = job.and_then(|j| j.worktree_branch.as_ref());
    live.or(worktree.filter(named))
        .or(job.filter(named))
        .or(facts.and_then(|f| f.branch.as_ref()))
        .cloned()
}

fn worktree(facts: Option<&Facts>, job: Option<&Job>) -> Option<SafeText> {
    facts
        .and_then(|f| f.worktree.as_ref())
        .map(|w| w.name.clone())
        .or_else(|| {
            job.and_then(|j| j.worktree_path.as_ref())
                .and_then(|p| p.file_name())
                .map(|n| SafeText::redact(&n.to_string_lossy()))
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

pub fn detail(v: &SessionView<'_>, agents: &[AgentView<'_>], now: At) -> Detail {
    let facts = v.facts;
    let mut pending: Vec<SafeText> = roster(v)
        .into_iter()
        .chain(sidecar_work(v, agents).items)
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
        pending.push(SafeText::redact(&format!("wakeup · {}", clock(at))));
    }
    Detail {
        last_prompt: facts.and_then(|f| f.last_prompt.clone()),
        last_text: facts
            .and_then(|f| f.last_text.as_ref())
            .map(|t| t.first_line.clone()),
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

pub fn open_target(v: &SessionView<'_>) -> OpenTarget {
    if let Some(job) = &v.reg.job {
        return OpenTarget::Attach {
            dir: v.dir.clone(),
            job: job.clone(),
        };
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

// `World::view` attaches a team only when `Team::includes` says the session leads it or sits
// in one of its panes, so a team on the view is the group.
pub fn auto_group(v: &SessionView<'_>) -> AutoGroup {
    if let Some(team) = v.team {
        return AutoGroup::Team(team.name.clone());
    }
    match v.repo {
        Some(root) => AutoGroup::Repo(root.to_path_buf()),
        None => AutoGroup::Elsewhere,
    }
}

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

fn clock(at: At) -> String {
    chrono::DateTime::from_timestamp_millis(at.0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

pub fn job_outcome(job: &Job) -> JobOutcome {
    match job.state {
        JobState::Done => JobOutcome::Done,
        JobState::Failed => JobOutcome::Failed,
        JobState::Stopped => JobOutcome::Stopped,
        _ => JobOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{JobId, PaneId, Pid, ProcStart, SessionId, ToolUseId};
    use crate::sidecars::TeamMember;
    use crate::transcript::{BgItem, ExitTotals, Horizon, TextBlock, Titles, ToolCall, Usage};

    const T0: At = At(1_000_000);

    fn at(offset_s: i64) -> At {
        At(T0.0 + offset_s * 1000)
    }

    fn reg(status: RegStatus) -> RegEntry {
        RegEntry {
            pid: Pid(4242),
            proc_start: ProcStart::new("Sun Oct  4 21:17:02 2026"),
            session_id: SessionId("0f1e2d3c-aaaa-bbbb-cccc-000000000001".into()),
            cwd: PathBuf::from("/work/repo"),
            kind: ProcKind::Interactive,
            entrypoint: "cli".into(),
            status,
            status_since: at(0),
            name: Some(Name {
                value: "repo-e7".into(),
                source: NameSource::Derived,
            }),
            pane: PaneId::parse("0:@1.%7"),
            remote_control: false,
            job: None,
            hidden: false,
        }
    }

    fn bg_reg(status: RegStatus) -> RegEntry {
        RegEntry {
            kind: ProcKind::Bg,
            pane: None,
            job: Some(JobId("0a1b2c3d".into())),
            ..reg(status)
        }
    }

    fn job() -> Job {
        Job {
            id: JobId("0a1b2c3d".into()),
            session_id: None,
            state: JobState::Working,
            tempo: Some(Tempo::Active),
            detail: None,
            needs: None,
            fan: Vec::new(),
            transcript: None,
            worktree_path: None,
            worktree_branch: None,
            name: None,
            remote_control: false,
            prs: Vec::new(),
            updated_at: None,
            ended_at: None,
        }
    }

    fn fan(id: &str, kind: FanKind, done: bool) -> FanItem {
        FanItem {
            id: id.into(),
            kind,
            label: SafeText::redact(&format!("fan {id}")),
            started_at: Some(at(-60)),
            done_at: done.then_some(at(-5)),
            failed: false,
        }
    }

    fn bg(kind: BgKind, label: &str) -> BgItem {
        BgItem {
            kind,
            label: SafeText::redact(label),
            started: at(-30),
            tool_use: None,
        }
    }

    fn tool(name: &str, summary: &str, answered: bool, t: At) -> ToolCall {
        ToolCall {
            id: ToolUseId("toolu_1".into()),
            name: name.into(),
            summary: SafeText::redact(summary),
            at: t,
            answered,
        }
    }

    fn text(s: &str, t: At) -> TextBlock {
        TextBlock {
            first_line: SafeText::redact(s),
            at: t,
            is_question: s.ends_with('?'),
        }
    }

    fn agent_meta(id: &str, parent: Option<&str>) -> AgentMeta {
        AgentMeta {
            id: AgentId(id.into()),
            agent_type: SafeText::ours("Explore"),
            description: SafeText::redact(&format!("agent {id}")),
            tool_use: None,
            parent: parent.map(|p| AgentId(p.into())),
            model: Some(SafeText::ours("sonnet")),
            stopped_by_user: false,
            workflow_run: None,
        }
    }

    struct Fx {
        dir: ConfigDir,
        reg: RegEntry,
        job: Option<Job>,
        facts: Option<Facts>,
        tasks: Vec<Task>,
        statusline: Option<Statusline>,
        team: Option<Team>,
        repo: Option<PathBuf>,
        panes: Vec<PaneInfo>,
    }

    impl Fx {
        fn new(reg: RegEntry) -> Fx {
            Fx {
                dir: ConfigDir(PathBuf::from("/home/u/.claude")),
                reg,
                job: None,
                facts: None,
                tasks: Vec::new(),
                statusline: None,
                team: None,
                repo: None,
                panes: Vec::new(),
            }
        }
        fn with_facts(mut self, f: impl FnOnce(&mut Facts)) -> Fx {
            let mut facts = Facts::default();
            f(&mut facts);
            self.facts = Some(facts);
            self
        }
        fn view(&self) -> SessionView<'_> {
            SessionView {
                dir: &self.dir,
                reg: &self.reg,
                job: self.job.as_ref(),
                facts: self.facts.as_ref(),
                subagents: None,
                tasks: &self.tasks,
                statusline: self.statusline.as_ref(),
                team: self.team.as_ref(),
                repo: self.repo.as_deref(),
                panes: &self.panes,
            }
        }
        fn status(&self) -> (Status, At) {
            status(&self.view(), &[], T0)
        }
        fn parked(&self) -> Parked {
            match self.status().0 {
                Status::Parked(p) => p,
                other => panic!("expected Parked, got {other:?}"),
            }
        }
    }

    #[test]
    fn waiting_permission_names_the_dangling_tool_and_shows_it_on_the_now_line() {
        let fx = Fx::new(reg(RegStatus::Waiting(WaitingFor::Permission))).with_facts(|f| {
            f.last_tool = Some(tool("Bash", "Run migration against staging", false, at(-3)))
        });
        let (st, since) = fx.status();
        assert_eq!(
            st,
            Status::NeedsYou(Need::Permission {
                tool: Some("Bash".into())
            })
        );
        assert_eq!(since, fx.reg.status_since);
        assert_eq!(
            now_line(&st, &fx.view(), T0).as_str(),
            "permission prompt · Bash: Run migration against staging"
        );
    }

    #[test]
    fn waiting_permission_with_an_answered_tool_names_no_tool() {
        let fx = Fx::new(reg(RegStatus::Waiting(WaitingFor::Permission)))
            .with_facts(|f| f.last_tool = Some(tool("Read", "main.rs", true, at(-3))));
        let (st, _) = fx.status();
        assert_eq!(st, Status::NeedsYou(Need::Permission { tool: None }));
        assert_eq!(now_line(&st, &fx.view(), T0).as_str(), "permission prompt");
    }

    #[test]
    fn waiting_for_any_other_reason_keeps_the_registry_reason() {
        for (w, shown) in [
            (WaitingFor::Input, "input needed"),
            (WaitingFor::Unspecified, "waiting"),
            (WaitingFor::Other("goal review".into()), "goal review"),
        ] {
            let fx = Fx::new(reg(RegStatus::Waiting(w.clone())));
            let (st, _) = fx.status();
            assert_eq!(st, Status::NeedsYou(Need::Waiting(w)));
            assert_eq!(now_line(&st, &fx.view(), T0).as_str(), shown);
        }
    }

    #[test]
    fn busy_with_no_transcript_is_working() {
        assert_eq!(
            Fx::new(reg(RegStatus::Busy)).status(),
            (Status::Working, at(0))
        );
    }

    #[test]
    fn busy_with_an_open_turn_is_working_since_the_turn_opened() {
        let fx =
            Fx::new(reg(RegStatus::Busy)).with_facts(|f| f.turn = Turn::Open { since: at(40) });
        assert_eq!(fx.status(), (Status::Working, at(40)));
    }

    #[test]
    fn busy_with_a_closed_turn_is_parked_and_its_since_is_the_turn_end_not_status_updated_at() {
        let fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.turn = Turn::Closed {
                at: at(90),
                pending_agents: 2,
            };
        });
        let (st, since) = fx.status();
        assert!(matches!(st, Status::Parked(_)), "{st:?}");
        assert_eq!(since, at(90));
        assert_ne!(since, fx.reg.status_since);
    }

    #[test]
    fn busy_that_began_after_the_turn_closed_is_working_until_the_prompt_lands() {
        // The registry turns busy the moment a prompt is submitted; the transcript shows the
        // opener only on the next tick. Status writes dedupe on [status, waitingFor], so a
        // status_since newer than the close can only mean a new turn.
        let mut fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.turn = Turn::Closed {
                at: at(90),
                pending_agents: 0,
            };
        });
        fx.reg.status_since = at(100);
        assert_eq!(fx.status(), (Status::Working, at(100)));
        fx.facts.as_mut().unwrap().turn = Turn::Interrupted { at: at(90) };
        assert_eq!(fx.status(), (Status::Working, at(100)));
    }

    #[test]
    fn busy_after_an_interrupt_is_parked_since_the_interrupt() {
        let fx =
            Fx::new(reg(RegStatus::Busy)).with_facts(|f| f.turn = Turn::Interrupted { at: at(12) });
        let (st, since) = fx.status();
        assert!(matches!(st, Status::Parked(_)), "{st:?}");
        assert_eq!(since, at(12));
    }

    #[test]
    fn busy_with_no_turn_boundary_in_the_window_follows_the_tail() {
        let streaming = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.last_tool = Some(tool("Bash", "Run tests", false, at(-2)));
        });
        assert_eq!(streaming.status().0, Status::Working);

        let ended = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.last_tool = Some(tool("Bash", "Run tests", true, at(-20)));
            f.last_text = Some(text("Started 2 agents.", at(-10)));
            f.tail_looks_finished = true;
        });
        assert!(matches!(ended.status().0, Status::Parked(_)));
    }

    #[test]
    fn shell_with_no_visible_launch_is_parked_on_at_least_one_shell() {
        let fx = Fx::new(reg(RegStatus::Shell)).with_facts(|_| {});
        let p = fx.parked();
        assert_eq!(
            (p.shells(), p.agents(), p.monitors()),
            (Count::AtLeast(1), Count::Exact(0), Count::AtLeast(0))
        );
        assert_eq!(
            now_line(&Status::Parked(p), &fx.view(), T0).as_str(),
            "waiting on 1+ shell"
        );
        assert_eq!(fx.status().1, fx.reg.status_since);
    }

    #[test]
    fn shell_trusts_the_registry_over_a_done_job_state() {
        let mut fx = Fx::new(bg_reg(RegStatus::Shell));
        fx.job = Some(Job {
            state: JobState::Done,
            tempo: Some(Tempo::Idle),
            fan: vec![fan("b1", FanKind::Shell, false)],
            ..job()
        });
        assert_eq!(fx.parked().shells(), Count::Exact(1));
    }

    #[test]
    fn shell_never_shows_agents_from_a_stale_fold() {
        let fx = Fx::new(reg(RegStatus::Shell)).with_facts(|f| {
            f.horizon = Horizon {
                from_start: true,
                first_at: None,
            };
            f.open_bg
                .insert(TaskId("a1".into()), bg(BgKind::Agent, "stale"));
            f.open_bg
                .insert(TaskId("a2".into()), bg(BgKind::Agent, "stale"));
            f.open_bg
                .insert(TaskId("b1".into()), bg(BgKind::Monitor, "watch CI"));
        });
        let p = fx.parked();
        assert_eq!(
            (p.agents(), p.monitors(), p.shells()),
            (Count::Exact(0), Count::Exact(1), Count::Exact(0))
        );
    }

    #[test]
    fn idle_bg_job_blocked_with_a_question_needs_you() {
        let mut fx = Fx::new(bg_reg(RegStatus::Idle));
        fx.job = Some(Job {
            tempo: Some(Tempo::Blocked),
            needs: Some(SafeText::ours("Ship to prod?")),
            ..job()
        });
        let (st, _) = fx.status();
        assert_eq!(
            st,
            Status::NeedsYou(Need::JobQuestion(SafeText::ours("Ship to prod?")))
        );
        assert_eq!(now_line(&st, &fx.view(), T0).as_str(), "Ship to prod?");
    }

    #[test]
    fn job_needs_are_ignored_unless_the_registry_says_idle() {
        for status in [RegStatus::Busy, RegStatus::Shell] {
            let mut fx = Fx::new(bg_reg(status)).with_facts(|f| {
                f.turn = Turn::Closed {
                    at: at(5),
                    pending_agents: 1,
                }
            });
            fx.job = Some(Job {
                tempo: Some(Tempo::Blocked),
                needs: Some(SafeText::ours("Old question?")),
                ..job()
            });
            assert!(matches!(fx.status().0, Status::Parked(_)));
        }
        let mut blocked_without_question = Fx::new(bg_reg(RegStatus::Idle));
        blocked_without_question.job = Some(Job {
            tempo: Some(Tempo::Blocked),
            ..job()
        });
        assert_eq!(
            blocked_without_question.status().0,
            Status::Idle {
                asked_question: false
            }
        );
    }

    #[test]
    fn idle_with_a_future_wakeup_is_parked_on_the_wakeup_only() {
        let fx = Fx::new(reg(RegStatus::Idle)).with_facts(|f| {
            f.wakeup = Some(at(600));
            f.open_bg
                .insert(TaskId("b1".into()), bg(BgKind::Shell, "stale"));
        });
        let p = fx.parked();
        assert_eq!(p.wakeup(), Some(at(600)));
        assert!(!p.shells().is_positive() && !p.agents().is_positive());

        let past = Fx::new(reg(RegStatus::Idle)).with_facts(|f| f.wakeup = Some(at(-1)));
        assert_eq!(
            past.status().0,
            Status::Idle {
                asked_question: false
            }
        );
    }

    #[test]
    fn idle_ending_on_a_question_is_idle_asked_question_never_needs_you() {
        let fx = Fx::new(reg(RegStatus::Idle)).with_facts(|f| {
            f.turn = Turn::Closed {
                at: at(-10),
                pending_agents: 0,
            };
            f.last_text = Some(text("Want me to wire the statusline tee now?", at(-11)));
        });
        assert_eq!(
            fx.status(),
            (
                Status::Idle {
                    asked_question: true
                },
                at(0)
            )
        );

        let statement = Fx::new(reg(RegStatus::Idle)).with_facts(|f| {
            f.turn = Turn::Closed {
                at: at(-10),
                pending_agents: 0,
            };
            f.last_text = Some(text("Done.", at(-11)));
        });
        assert_eq!(
            statement.status().0,
            Status::Idle {
                asked_question: false
            }
        );
    }

    #[test]
    fn unknown_registry_status_is_idle_and_reported() {
        let fx = Fx::new(reg(RegStatus::Other("napping".into())));
        assert_eq!(
            fx.status().0,
            Status::Idle {
                asked_question: false
            }
        );
        let snap = snapshot(
            [fx.view()].into_iter(),
            std::iter::empty(),
            std::iter::empty(),
            &[],
            T0,
        );
        assert_eq!(snap.rows.len(), 1);
        assert_eq!(
            snap.diagnostics
                .iter()
                .map(|d| (&d.source, &d.problem))
                .collect::<Vec<_>>(),
            [(
                &DiagSource::Registry,
                &Problem::UnknownValue {
                    field: "status",
                    value: "napping".into()
                }
            )]
        );
    }

    #[test]
    fn bg_fan_roster_wins_over_a_stale_fold() {
        let mut fx = Fx::new(bg_reg(RegStatus::Busy)).with_facts(|f| {
            f.turn = Turn::Closed {
                at: at(5),
                pending_agents: 0,
            };
            for id in ["b1", "b2", "b3"] {
                f.open_bg
                    .insert(TaskId(id.into()), bg(BgKind::Shell, "stale"));
            }
        });
        fx.job = Some(Job {
            fan: vec![
                fan("a1", FanKind::Agent, false),
                fan("b9", FanKind::Shell, true),
                fan("todo:1", FanKind::Todo, false),
            ],
            ..job()
        });
        let p = fx.parked();
        assert_eq!((p.agents(), p.shells()), (Count::Exact(1), Count::Exact(0)));
    }

    #[test]
    fn pending_background_agents_beat_a_smaller_fold_count() {
        let fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.turn = Turn::Closed {
                at: at(5),
                pending_agents: 3,
            };
            f.open_bg
                .insert(TaskId("a1".into()), bg(BgKind::Agent, "one we saw"));
        });
        assert_eq!(fx.parked().agents(), Count::AtLeast(3));
    }

    #[test]
    fn running_tree_agents_count_toward_parked_agents() {
        let fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.turn = Turn::Closed {
                at: at(5),
                pending_agents: 0,
            }
        });
        let (m1, m2, m3) = (
            agent_meta("a1", None),
            agent_meta("a2", None),
            agent_meta("a3", None),
        );
        let agents: Vec<AgentView<'_>> = vec![
            (&m1, NodeState::Running, None),
            (&m2, NodeState::Running, None),
            (&m3, NodeState::Finished(TaskOutcome::Completed), None),
        ];
        assert_eq!(parked(&fx.view(), &agents, false, T0).agents().n(), 2);
    }

    #[test]
    fn busy_with_a_closed_turn_and_no_evidence_is_parked_on_at_least_one_agent() {
        let fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.turn = Turn::Closed {
                at: at(5),
                pending_agents: 0,
            }
        });
        assert_eq!(fx.parked().agents(), Count::AtLeast(1));
    }

    #[test]
    fn a_fold_that_saw_the_whole_file_reports_exact_counts() {
        let fx = Fx::new(reg(RegStatus::Shell)).with_facts(|f| {
            f.horizon = Horizon {
                from_start: true,
                first_at: None,
            };
            f.open_bg
                .insert(TaskId("b1".into()), bg(BgKind::Shell, "dev server"));
            f.open_bg
                .insert(TaskId("b2".into()), bg(BgKind::Shell, "watcher"));
        });
        assert_eq!(fx.parked().shells(), Count::Exact(2));
    }

    fn whole(f: &mut Facts) {
        f.horizon = Horizon {
            from_start: true,
            first_at: None,
        };
    }

    #[test]
    fn shells_launched_inside_running_subagents_count_and_are_exact_only_when_fully_read() {
        let fx = Fx::new(reg(RegStatus::Shell)).with_facts(|f| {
            whole(f);
            f.open_bg
                .insert(TaskId("bmain0001".into()), bg(BgKind::Shell, "dev server"));
        });
        let mut side = Facts::default();
        whole(&mut side);
        side.open_bg
            .insert(TaskId("bside0002".into()), bg(BgKind::Shell, "watch CI"));
        side.open_bg.insert(
            TaskId("bside0003".into()),
            bg(BgKind::Monitor, "watch deploy"),
        );
        let (m1, m2) = (agent_meta("a1", None), agent_meta("a2", None));

        let read: Vec<AgentView<'_>> = vec![(&m1, NodeState::Running, Some(&side))];
        let p = parked(&fx.view(), &read, true, T0);
        assert_eq!(
            (p.shells(), p.monitors()),
            (Count::Exact(2), Count::Exact(1))
        );
        assert_eq!(describe_parked(&p), "2 shells, 1 monitor");

        let unread: Vec<AgentView<'_>> = vec![
            (&m1, NodeState::Running, Some(&side)),
            (&m2, NodeState::Running, None),
        ];
        let p = parked(&fx.view(), &unread, true, T0);
        assert_eq!(
            (p.shells(), p.monitors()),
            (Count::AtLeast(2), Count::AtLeast(1)),
            "a running agent whose sidecar was not read makes the counts lower bounds"
        );

        let mut partial = side.clone();
        partial.horizon = Horizon {
            from_start: false,
            first_at: Some(at(-10)),
        };
        let windowed: Vec<AgentView<'_>> = vec![(&m1, NodeState::Running, Some(&partial))];
        assert_eq!(
            parked(&fx.view(), &windowed, true, T0).shells(),
            Count::AtLeast(2),
            "a sidecar window that missed the file's start is a lower bound too"
        );

        let finished: Vec<AgentView<'_>> = vec![(
            &m1,
            NodeState::Finished(TaskOutcome::Completed),
            Some(&side),
        )];
        assert_eq!(
            parked(&fx.view(), &finished, true, T0).shells(),
            Count::Exact(1),
            "a finished agent's stale sidecar adds nothing"
        );
    }

    #[test]
    fn sidecar_shells_hang_under_their_agent_in_the_tree_and_join_the_pending_list() {
        let fx = Fx::new(reg(RegStatus::Shell)).with_facts(|f| {
            f.open_bg
                .insert(TaskId("a1".into()), bg(BgKind::Agent, "audit"));
        });
        let mut side = Facts::default();
        side.open_bg
            .insert(TaskId("bside0002".into()), bg(BgKind::Shell, "watch CI"));
        side.last_tool = Some(tool("Bash", "watch CI", true, at(-1)));
        let m = agent_meta("a1", None);
        let agents: Vec<AgentView<'_>> = vec![(&m, NodeState::Running, Some(&side))];
        let t = tree(&fx.view(), &agents, T0);
        assert_eq!(t.live.len(), 1);
        let leaves: Vec<(&str, &NodeKind, &str)> = t.live[0]
            .children
            .live
            .iter()
            .map(|n| (n.id.0.as_str(), &n.kind, n.label.as_str()))
            .collect();
        assert_eq!(leaves, [("bside0002", &NodeKind::Shell, "watch CI")]);
        assert_eq!(t.live[0].children.live[0].since, at(-30));
        let d = detail(&fx.view(), &agents, T0);
        assert_eq!(
            d.pending,
            [
                SafeText::ours("agent · audit"),
                SafeText::ours("shell · watch CI")
            ]
        );

        let mut bg_fx = Fx::new(bg_reg(RegStatus::Shell));
        bg_fx.job = Some(Job {
            fan: vec![
                fan("a1", FanKind::Agent, false),
                fan("bside0002", FanKind::Shell, false),
            ],
            ..job()
        });
        let t = tree(&bg_fx.view(), &agents, T0);
        assert!(
            t.live[0].children.is_empty(),
            "the fan already lists the shell at the top level"
        );
        assert_eq!(detail(&bg_fx.view(), &agents, T0).pending.len(), 2);
    }

    #[test]
    fn describe_parked_pluralises_marks_lower_bounds_and_omits_zeros() {
        let p = Parked::new(Count::Exact(3), Count::AtLeast(1), Count::Exact(0), 1, None).unwrap();
        assert_eq!(describe_parked(&p), "3 agents, 1+ shell, 1 workflow");
        let p = Parked::new(
            Count::Exact(1),
            Count::Exact(0),
            Count::Exact(2),
            0,
            Some(at(60)),
        )
        .unwrap();
        let s = describe_parked(&p);
        assert!(s.starts_with("1 agent, 2 monitors · wakes "), "{s}");
    }

    #[test]
    fn title_precedence_custom_agent_name_strong_registry_name_ai_job_name_any_name_short_sid() {
        let mut fx = Fx::new(bg_reg(RegStatus::Idle)).with_facts(|f| {
            f.titles = Titles {
                custom: Some(SafeText::ours("custom")),
                agent_name: Some(SafeText::ours("agent-name")),
                ai: Some(SafeText::ours("ai")),
            };
        });
        fx.reg.name = Some(Name {
            value: "user-name".into(),
            source: NameSource::User,
        });
        fx.job = Some(Job {
            name: Some("job-name".into()),
            ..job()
        });

        let mut seen = Vec::new();
        let titles = |fx: &Fx| fx.facts.as_ref().unwrap().titles.clone();
        seen.push(title(&fx.view()));
        fx.facts.as_mut().unwrap().titles = Titles {
            custom: None,
            ..titles(&fx)
        };
        seen.push(title(&fx.view()));
        fx.facts.as_mut().unwrap().titles = Titles {
            agent_name: None,
            ..titles(&fx)
        };
        seen.push(title(&fx.view()));
        fx.reg.name = Some(Name {
            value: "derived-e7".into(),
            source: NameSource::Derived,
        });
        seen.push(title(&fx.view()));
        fx.facts.as_mut().unwrap().titles = Titles {
            ai: None,
            ..titles(&fx)
        };
        seen.push(title(&fx.view()));
        fx.job = Some(job());
        seen.push(title(&fx.view()));
        fx.reg.name = None;
        seen.push(title(&fx.view()));

        let seen: Vec<&str> = seen.iter().map(SafeText::as_str).collect();
        assert_eq!(
            seen,
            [
                "custom",
                "agent-name",
                "user-name",
                "ai",
                "job-name",
                "derived-e7",
                "0f1e2d3c"
            ]
        );
    }

    #[test]
    fn title_fallback_takes_eight_characters_of_a_non_ascii_session_id() {
        let mut fx = Fx::new(reg(RegStatus::Idle));
        fx.reg.name = None;
        fx.reg.session_id = SessionId("日本語テスト-0000".into());
        assert_eq!(title(&fx.view()).as_str(), "日本語テスト-0");
    }

    #[test]
    fn auto_named_registry_title_beats_ai_title() {
        let mut fx =
            Fx::new(reg(RegStatus::Idle)).with_facts(|f| f.titles.ai = Some(SafeText::ours("ai")));
        fx.reg.name = Some(Name {
            value: "auto-name".into(),
            source: NameSource::Auto,
        });
        assert_eq!(title(&fx.view()).as_str(), "auto-name");
    }

    #[test]
    fn working_now_line_is_the_latest_action() {
        let fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.turn = Turn::Open { since: at(-60) };
            f.last_text = Some(text("Let me check the tests.", at(-20)));
            f.last_tool = Some(tool("Edit", "stack_plan.rs", false, at(-5)));
        });
        assert_eq!(
            now_line(&Status::Working, &fx.view(), T0).as_str(),
            "Edit: stack_plan.rs"
        );
    }

    #[test]
    fn idle_now_line_prefers_away_summary_then_last_text_then_last_prompt() {
        let idle = Status::Idle {
            asked_question: false,
        };
        let mut fx = Fx::new(reg(RegStatus::Idle)).with_facts(|f| {
            f.away_summary = Some(SafeText::ours("Fee logic done; client-side next"));
            f.last_text = Some(text("All green.", at(-5)));
            f.last_prompt = Some(SafeText::ours("fix the fees"));
        });
        assert_eq!(
            now_line(&idle, &fx.view(), T0).as_str(),
            "Fee logic done; client-side next"
        );
        fx.facts.as_mut().unwrap().away_summary = None;
        assert_eq!(now_line(&idle, &fx.view(), T0).as_str(), "All green.");
        fx.facts.as_mut().unwrap().last_text = None;
        assert_eq!(now_line(&idle, &fx.view(), T0).as_str(), "fix the fees");
    }

    #[test]
    fn bg_job_detail_replaces_the_now_line_only_while_fresh() {
        let mut fx = Fx::new(bg_reg(RegStatus::Busy)).with_facts(|f| {
            f.turn = Turn::Open { since: at(-60) };
            f.last_tool = Some(tool("Grep", "broken anchors", false, at(-5)));
        });
        fx.job = Some(Job {
            detail: Some(SafeText::ours("auditing links")),
            updated_at: Some(at(-60)),
            ..job()
        });
        assert_eq!(
            now_line(&Status::Working, &fx.view(), T0).as_str(),
            "auditing links"
        );
        fx.job.as_mut().unwrap().updated_at = Some(at(-40 * 60));
        assert_eq!(
            now_line(&Status::Working, &fx.view(), T0).as_str(),
            "Grep: broken anchors"
        );
    }

    fn usage_at(t: At) -> Usage {
        Usage {
            ctx_tokens: 412_000,
            model: "claude-opus-5-5".into(),
            at: t,
        }
    }

    fn statusline(t: At, window: Option<u64>) -> Statusline {
        Statusline {
            ctx_used_pct: Some(41.2),
            window,
            cost_usd: Some(3.41),
            limits: None,
            at: t,
        }
    }

    #[test]
    fn never_shows_a_cost_or_percent_without_an_authoritative_source() {
        let no_statusline =
            Fx::new(reg(RegStatus::Busy)).with_facts(|f| f.usage = Some(usage_at(at(-5))));
        let m = meta(&no_statusline.view());
        assert_eq!((m.ctx, m.cost), (Some(Ctx::Tokens(412_000)), None));

        let mut no_window =
            Fx::new(reg(RegStatus::Busy)).with_facts(|f| f.usage = Some(usage_at(at(-5))));
        no_window.statusline = Some(statusline(at(-1), None));
        assert_eq!(meta(&no_window.view()).ctx, Some(Ctx::Tokens(412_000)));

        let mut stale =
            Fx::new(reg(RegStatus::Busy)).with_facts(|f| f.usage = Some(usage_at(at(-5))));
        stale.statusline = Some(statusline(at(-300), Some(1_000_000)));
        let m = meta(&stale.view());
        assert_eq!((m.ctx, m.cost), (Some(Ctx::Tokens(412_000)), None));

        let nothing = Fx::new(reg(RegStatus::Busy));
        let m = meta(&nothing.view());
        assert_eq!((m.ctx, m.cost), (None, None));
    }

    fn win(used_pct: f64, resets_at: At) -> Window {
        Window {
            used_pct,
            resets_at,
        }
    }

    fn seen(five_hour: Option<Window>, seven_day: Option<Window>, at: At) -> Limits {
        Limits {
            five_hour,
            seven_day,
            at,
        }
    }

    #[test]
    fn account_usage_takes_each_window_from_its_newest_observation() {
        let mut fx = Fx::new(reg(RegStatus::Idle));
        // A hidden spare still reports its account's limits.
        fx.reg.hidden = true;
        fx.statusline = Some(Statusline {
            limits: Some(seen(Some(win(10.0, at(3600))), None, at(-1))),
            ..statusline(at(-1), None)
        });
        let cached = seen(
            Some(win(90.0, at(-60))),
            Some(win(40.0, at(86_400))),
            at(-86_400),
        );
        let snap = snapshot(
            [fx.view()].into_iter(),
            std::iter::empty(),
            [(&fx.dir, cached)].into_iter(),
            &[],
            T0,
        );
        assert!(snap.rows.is_empty());
        assert_eq!(
            snap.limits[&fx.dir],
            seen(
                Some(win(10.0, at(3600))),
                Some(win(40.0, at(86_400))),
                at(-1)
            )
        );
    }

    #[test]
    fn roomiest_skips_full_accounts_then_prefers_7d_then_5h_room() {
        let (a, b) = (
            ConfigDir("/h/.claude-a".into()),
            ConfigDir("/h/.claude-b".into()),
        );
        let pick = |ua: Limits, ub: Limits| {
            let usage = BTreeMap::from([(a.clone(), ua), (b.clone(), ub)]);
            roomiest(&[a.clone(), b.clone()], &usage, T0)
        };
        let later = at(3600);
        // a's 5-hour window is full: b wins despite more 7-day use.
        assert_eq!(
            pick(
                seen(Some(win(100.0, later)), Some(win(10.0, later)), T0),
                seen(Some(win(5.0, later)), Some(win(80.0, later)), T0)
            ),
            1
        );
        // a's full 7-day window reset since it was seen, so a is empty and wins.
        assert_eq!(
            pick(
                seen(None, Some(win(97.0, at(-60))), T0),
                seen(None, Some(win(52.0, later)), T0)
            ),
            0
        );
        // Equal 7-day use: the 5-hour window decides.
        assert_eq!(
            pick(
                seen(Some(win(60.0, later)), Some(win(30.0, later)), T0),
                seen(Some(win(20.0, later)), Some(win(30.0, later)), T0)
            ),
            1
        );
        // Both full: the one that frees up first.
        assert_eq!(
            pick(
                seen(None, Some(win(100.0, at(7200))), T0),
                seen(Some(win(100.0, at(600))), None, T0)
            ),
            1
        );
        // Never observed counts as empty, and ties keep the configured order.
        assert_eq!(roomiest(&[a.clone(), b.clone()], &BTreeMap::new(), T0), 0);
    }

    #[test]
    fn a_fresh_statusline_gives_percent_and_live_cost() {
        let mut fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| f.usage = Some(usage_at(at(-5))));
        fx.statusline = Some(statusline(at(-1), Some(1_000_000)));
        let m = meta(&fx.view());
        assert_eq!(
            (m.ctx, m.cost),
            (Some(Ctx::Percent(41)), Some(Cost::Live(3.41)))
        );

        fx.statusline = Some(Statusline {
            ctx_used_pct: None,
            ..statusline(at(-1), Some(1_000_000))
        });
        assert_eq!(meta(&fx.view()).ctx, Some(Ctx::Percent(41)));
    }

    #[test]
    fn a_trailing_cost_state_gives_a_final_cost() {
        let mut facts = Facts::default();
        facts.exit = Some(ExitTotals { cost_usd: 2.17 });
        let dir = ConfigDir(PathBuf::from("/home/u/.claude"));
        let j = Job {
            state: JobState::Done,
            ended_at: Some(at(-3600)),
            ..job()
        };
        let row = exited_row(ExitedView {
            dir: &dir,
            job: &j,
            facts: Some(&facts),
        });
        assert_eq!(row.meta.cost, Some(Cost::Final(2.17)));
        assert_eq!(row.status, Status::Exited(JobOutcome::Done));
        assert_eq!(row.auto_group, AutoGroup::Finished);
        assert_eq!(row.since, at(-3600));
    }

    #[test]
    fn detached_head_falls_back_to_the_worktree_branch() {
        // Defect: a detached worktree checkout shown as branch "HEAD".
        let mut fx = Fx::new(bg_reg(RegStatus::Idle))
            .with_facts(|f| f.branch = Some(SafeText::ours("HEAD")));
        fx.job = Some(Job {
            worktree_branch: Some(SafeText::ours("feat/live-scores")),
            ..job()
        });
        assert_eq!(
            meta(&fx.view()).branch,
            Some(SafeText::ours("feat/live-scores"))
        );
        fx.job = None;
        assert_eq!(meta(&fx.view()).branch, Some(SafeText::ours("HEAD")));
    }

    #[test]
    fn meta_merges_prs_falls_back_for_branch_and_flags_remote_control_and_dir() {
        let mut fx = Fx::new(bg_reg(RegStatus::Idle)).with_facts(|f| f.prs = vec![4790, 4812]);
        fx.dir = ConfigDir(PathBuf::from("/home/u/.claude-personal"));
        fx.job = Some(Job {
            prs: vec![4812, 4900],
            worktree_branch: Some(SafeText::ours("docs/onboarding")),
            worktree_path: Some(PathBuf::from("/work/repo/.claude/worktrees/docs")),
            remote_control: true,
            ..job()
        });
        let m = meta(&fx.view());
        assert_eq!(m.prs, [4790, 4812, 4900]);
        assert_eq!(m.branch, Some(SafeText::ours("docs/onboarding")));
        assert_eq!(m.worktree, Some(SafeText::ours("docs")));
        assert!(m.remote_control && m.bg);
        assert_eq!(m.dir_tag, "personal");
    }

    #[test]
    fn tree_nests_depth_two_agents_and_collapses_finished_per_level() {
        let fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.open_bg
                .insert(TaskId("b1".into()), bg(BgKind::Monitor, "Watch CI"));
        });
        let ms = [
            agent_meta("a1", None),
            agent_meta("a2", None),
            agent_meta("a3", None),
            agent_meta("c1", Some("a1")),
            agent_meta("c2", Some("a1")),
            agent_meta("c3", Some("a1")),
        ];
        let agents: Vec<AgentView<'_>> = vec![
            (&ms[0], NodeState::Running, None),
            (&ms[1], NodeState::Finished(TaskOutcome::Completed), None),
            (&ms[2], NodeState::Killed, None),
            (&ms[3], NodeState::Running, None),
            (&ms[4], NodeState::Finished(TaskOutcome::Failed), None),
            (&ms[5], NodeState::Orphaned, None),
        ];
        let t = tree(&fx.view(), &agents, T0);
        let top: Vec<(&str, NodeState)> =
            t.live.iter().map(|n| (n.id.0.as_str(), n.state)).collect();
        assert_eq!(
            top,
            [("a1", NodeState::Running), ("b1", NodeState::Running)]
        );
        assert_eq!(t.finished, 2);
        let a1 = &t.live[0];
        assert_eq!(
            a1.children
                .live
                .iter()
                .map(|n| n.id.0.as_str())
                .collect::<Vec<_>>(),
            ["c1"]
        );
        assert_eq!(a1.children.finished, 2);
        assert_eq!(t.live[1].kind, NodeKind::Monitor);
    }

    #[test]
    fn running_agent_shows_its_sidecar_action_and_launch_time() {
        let fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.open_bg
                .insert(TaskId("a1".into()), bg(BgKind::Agent, "audit"));
        });
        let mut side = Facts::default();
        side.last_tool = Some(tool("Read", "guide.md", false, at(-1)));
        let m = agent_meta("a1", None);
        let t = tree(&fx.view(), &[(&m, NodeState::Running, Some(&side))], T0);
        assert_eq!(t.live.len(), 1, "the open_bg agent must not appear twice");
        assert_eq!(
            t.live[0].now.as_ref().map(SafeText::as_str),
            Some("Read: guide.md")
        );
        assert_eq!(t.live[0].since, at(-30));
    }

    #[test]
    fn bg_tree_takes_liveness_and_leaves_from_the_fan() {
        let mut fx = Fx::new(bg_reg(RegStatus::Busy)).with_facts(|f| {
            f.open_bg
                .insert(TaskId("bstale".into()), bg(BgKind::Shell, "stale shell"));
        });
        fx.job = Some(Job {
            fan: vec![
                fan("a1", FanKind::Agent, true),
                fan("a2", FanKind::Agent, false),
                fan("a9", FanKind::Agent, false),
                fan("b1", FanKind::Shell, false),
                fan("todo:1", FanKind::Todo, false),
            ],
            ..job()
        });
        let (m1, m2) = (agent_meta("a1", None), agent_meta("a2", None));
        let agents: Vec<AgentView<'_>> = vec![
            (&m1, NodeState::Running, None),
            (&m2, NodeState::Orphaned, None),
        ];
        let t = tree(&fx.view(), &agents, T0);
        let live: Vec<(&str, NodeState)> =
            t.live.iter().map(|n| (n.id.0.as_str(), n.state)).collect();
        assert_eq!(
            live,
            [
                ("a2", NodeState::Running),
                ("a9", NodeState::Running),
                ("b1", NodeState::Running)
            ]
        );
        assert_eq!(t.finished, 1);
    }

    #[test]
    fn bg_shell_leaves_use_the_launch_description_over_the_fan_command() {
        let mut fx = Fx::new(bg_reg(RegStatus::Shell)).with_facts(|f| {
            f.open_bg.insert(
                TaskId("b1".into()),
                bg(BgKind::Shell, "Wait for CI on main"),
            );
        });
        fx.job = Some(Job {
            fan: vec![
                fan("b1", FanKind::Shell, false),
                fan("b2", FanKind::Shell, false),
            ],
            ..job()
        });
        let t = tree(&fx.view(), &[], T0);
        let labels: Vec<&str> = t.live.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels[0], "Wait for CI on main");
        assert_ne!(labels[1], "Wait for CI on main");
    }

    #[test]
    fn a_future_wakeup_is_a_tree_leaf() {
        let fx = Fx::new(reg(RegStatus::Idle)).with_facts(|f| f.wakeup = Some(at(300)));
        let t = tree(&fx.view(), &[], T0);
        assert_eq!(
            t.live.iter().map(|n| n.kind.clone()).collect::<Vec<_>>(),
            [NodeKind::Wakeup]
        );
    }

    #[test]
    fn open_target_offers_a_pane_only_when_tmux_lists_it() {
        let mut fx = Fx::new(reg(RegStatus::Idle));
        fx.panes = vec![PaneInfo {
            id: PaneId::parse("%7").unwrap(),
            pid: Pid(1),
        }];
        assert_eq!(
            open_target(&fx.view()),
            OpenTarget::Pane(PaneId::parse("%7").unwrap())
        );
        fx.panes = vec![PaneInfo {
            id: PaneId::parse("%8").unwrap(),
            pid: Pid(1),
        }];
        assert_eq!(
            open_target(&fx.view()),
            OpenTarget::Unreachable("tmux pane %7 is gone".into())
        );
    }

    #[test]
    fn open_target_outside_tmux_names_the_surface() {
        let mut fx = Fx::new(RegEntry {
            pane: None,
            entrypoint: "claude-vscode".into(),
            ..reg(RegStatus::Idle)
        });
        assert_eq!(
            open_target(&fx.view()),
            OpenTarget::Unreachable("running in VS Code".into())
        );
        fx.reg.entrypoint = "cli".into();
        assert_eq!(
            open_target(&fx.view()),
            OpenTarget::Unreachable("running in a terminal outside tmux".into())
        );
    }

    #[test]
    fn open_target_attaches_bg_jobs_live_or_exited() {
        let fx = Fx::new(bg_reg(RegStatus::Busy));
        let expected = OpenTarget::Attach {
            dir: fx.dir.clone(),
            job: JobId("0a1b2c3d".into()),
        };
        assert_eq!(open_target(&fx.view()), expected);

        let j = Job {
            name: Some("t".into()),
            ..job()
        };
        let row = exited_row(ExitedView {
            dir: &fx.dir,
            job: &j,
            facts: None,
        });
        assert_eq!(row.open, expected);
    }

    #[test]
    fn branch_worktree_agent_type_model_and_team_name_are_redacted_on_the_way_to_the_row() {
        const TOKEN: &str = "ghp_0123456789abcdefghijABCDEFGHIJ0123";
        let mut fx = Fx::new(reg(RegStatus::Busy)).with_facts(|f| {
            f.branch = Some(SafeText::redact(TOKEN));
            f.worktree = Some(crate::transcript::Worktree {
                path: PathBuf::from("/w/x"),
                name: SafeText::redact(TOKEN),
                branch: Some(SafeText::redact(TOKEN)),
            });
        });
        fx.team = Some(Team {
            name: SafeText::redact(TOKEN),
            lead: Some(fx.reg.session_id.clone()),
            members: Vec::new(),
        });
        let m = AgentMeta {
            agent_type: SafeText::redact(TOKEN),
            model: Some(SafeText::redact(TOKEN)),
            ..agent_meta("a1", None)
        };
        let agents: Vec<AgentView<'_>> = vec![(&m, NodeState::Running, None)];
        let shown = format!(
            "{:?} {:?} {:?}",
            meta(&fx.view()),
            tree(&fx.view(), &agents, T0),
            auto_group(&fx.view())
        );
        assert!(!shown.contains(TOKEN), "{shown}");
        let job_fx = Fx::new(bg_reg(RegStatus::Busy));
        let j = Job {
            worktree_branch: Some(SafeText::redact(TOKEN)),
            worktree_path: Some(PathBuf::from(format!("/w/{TOKEN}"))),
            ..job()
        };
        let shown = format!(
            "{:?}",
            meta(&SessionView {
                job: Some(&j),
                ..job_fx.view()
            })
        );
        assert!(!shown.contains(TOKEN), "{shown}");
    }

    #[test]
    fn auto_group_prefers_team_then_repo_then_elsewhere() {
        let mut fx = Fx::new(reg(RegStatus::Idle));
        assert_eq!(auto_group(&fx.view()), AutoGroup::Elsewhere);
        fx.repo = Some(PathBuf::from("/work/repo"));
        assert_eq!(
            auto_group(&fx.view()),
            AutoGroup::Repo(PathBuf::from("/work/repo"))
        );
        fx.team = Some(Team {
            name: SafeText::ours("session-0f1e2d3c"),
            lead: None,
            members: Vec::new(),
        });
        assert_eq!(
            auto_group(&fx.view()),
            AutoGroup::Team(SafeText::ours("session-0f1e2d3c"))
        );
    }

    #[test]
    fn a_team_includes_its_lead_and_its_pane_members_only() {
        let team = Team {
            name: SafeText::ours("t"),
            lead: Some(SessionId("lead".into())),
            members: vec![TeamMember {
                name: "w".into(),
                pane: PaneId::parse("%7"),
                active: true,
            }],
        };
        let pane = PaneId::parse("%7");
        let other = PaneId::parse("%8");
        assert!(team.includes(&SessionId("lead".into()), None));
        assert!(team.includes(&SessionId("member".into()), pane.as_ref()));
        assert!(!team.includes(&SessionId("outsider".into()), other.as_ref()));
        assert!(!team.includes(&SessionId("outsider".into()), None));
    }

    #[test]
    fn snapshot_hides_spares_keys_bg_rows_by_job_and_reports_unknown_job_values() {
        let hidden = Fx::new(RegEntry {
            hidden: true,
            ..reg(RegStatus::Idle)
        });
        let mut live_bg = Fx::new(bg_reg(RegStatus::Idle));
        live_bg.job = Some(Job {
            fan: vec![fan("x1", FanKind::Other("cron".into()), false)],
            ..job()
        });
        let dir = ConfigDir(PathBuf::from("/home/u/.claude"));
        let gone = Job {
            id: JobId("deadbeef".into()),
            state: JobState::Other("zombie".into()),
            ..job()
        };
        let snap = snapshot(
            [hidden.view(), live_bg.view()].into_iter(),
            [ExitedView {
                dir: &dir,
                job: &gone,
                facts: None,
            }]
            .into_iter(),
            std::iter::empty(),
            &[],
            T0,
        );
        let keys: Vec<&UserKey> = snap.rows.iter().map(|r| &r.user_key).collect();
        assert_eq!(
            keys,
            [
                &UserKey::Job(JobId("0a1b2c3d".into())),
                &UserKey::Job(JobId("deadbeef".into()))
            ]
        );
        let problems: Vec<&Problem> = snap.diagnostics.iter().map(|d| &d.problem).collect();
        assert_eq!(
            problems,
            [
                &Problem::UnknownValue {
                    field: "fan.kind",
                    value: "cron".into()
                },
                &Problem::UnknownValue {
                    field: "state",
                    value: "zombie".into()
                },
            ]
        );
        assert_eq!(snap.rows[1].status, Status::Exited(JobOutcome::Unknown));
    }

    #[test]
    fn detail_lists_open_tasks_and_pending_work() {
        let mut fx = Fx::new(reg(RegStatus::Shell)).with_facts(|f| {
            f.open_bg
                .insert(TaskId("b1".into()), bg(BgKind::Shell, "dev server"));
        });
        fx.tasks = vec![
            Task {
                id: 1,
                subject: SafeText::ours("write tests"),
                status: TaskStatus::InProgress,
            },
            Task {
                id: 2,
                subject: SafeText::ours("scaffold"),
                status: TaskStatus::Completed,
            },
        ];
        let d = detail(&fx.view(), &[], T0);
        assert_eq!(d.open_tasks, [SafeText::ours("write tests")]);
        assert_eq!(d.pending, [SafeText::ours("shell · dev server")]);
        assert_eq!(d.session_id, Some(fx.reg.session_id.clone()));
    }
}
