//! Boundary for `projects/<slug>/<sid>/subagents/` (flat at every depth; workflows under
//! `subagents/workflows/<runId>/`). Yields `AgentMeta` plus one sidecar `Transcript` per agent
//! that may still be running.
//!
//! Cost control: the dir listing is redone only when the dir's mtime changes (a new agent adds
//! files). Each agent jsonl is pumped (one stat) only while the agent is `Running`; any other
//! state drops its cursor but keeps the folded `Facts`, so the state rule still has the tail it
//! saw and a finished agent is never re-opened just to learn it is finished. Sessions with 100+
//! historical agents cost ~0 per refresh.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{AgentId, At, NodeState, Problem, SafeText, TaskId, TaskOutcome, ToolUseId};
use crate::transcript::{Facts, FileStamp, Horizon, Notice, Transcript, Window};

#[derive(Clone, Debug, PartialEq)]
pub struct AgentMeta {
    pub id: AgentId,
    pub agent_type: String,
    pub description: SafeText,
    /// The spawning Agent tool_use in the parent file (main transcript or parent agent's jsonl).
    pub tool_use: Option<ToolUseId>,
    /// None for depth 1 (parent is the session); Some for depth 2. Present iff depth >= 2.
    pub parent: Option<AgentId>,
    pub model: Option<String>,
    pub stopped_by_user: bool,
    /// Set for agents under `subagents/workflows/<runId>/`.
    pub workflow_run: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMeta {
    agent_type: Option<String>,
    description: Option<String>,
    tool_use_id: Option<String>,
    parent_agent_id: Option<String>,
    model: Option<String>,
    stopped_by_user: Option<bool>,
}

/// Pure parse of one `agent-<id>.meta.json`. The id comes from the filename.
pub fn parse_meta(
    id: AgentId,
    workflow_run: Option<String>,
    bytes: &[u8],
) -> Result<AgentMeta, Problem> {
    let w: WireMeta = serde_json::from_slice(bytes).map_err(|_| Problem::Unparsable)?;
    Ok(AgentMeta {
        id,
        agent_type: w.agent_type.unwrap_or_default(),
        description: SafeText::redact(w.description.as_deref().unwrap_or("")),
        tool_use: w.tool_use_id.map(ToolUseId),
        parent: w.parent_agent_id.map(AgentId),
        model: w.model,
        stopped_by_user: w.stopped_by_user.unwrap_or(false),
        workflow_run,
    })
}

/// One known agent: its meta (re-parsed when the file's mtime moves) and the sidecar facts
/// kept from the last time it had a cursor.
struct Entry {
    meta_path: PathBuf,
    meta_mtime_ms: i64,
    meta: AgentMeta,
    /// Facts folded while the agent had a cursor, kept after the cursor was dropped.
    kept: Option<Facts>,
}

/// Cached view of one session's subagents dir.
pub struct Subagents {
    dir: PathBuf,
    dir_mtime_ms: Option<i64>,
    entries: BTreeMap<AgentId, Entry>,
    /// Sidecar folds for Running agents only.
    open: BTreeMap<AgentId, Transcript>,
}

impl Subagents {
    /// `<transcript dir>/<sid>/subagents`. The dir may not exist yet; that is an empty set.
    pub fn new(main_transcript: &Path) -> Subagents {
        let dir = main_transcript.with_extension("").join("subagents");
        Subagents { dir, dir_mtime_ms: None, entries: BTreeMap::new(), open: BTreeMap::new() }
    }

    /// Re-list if the dir changed, re-parse changed metas, then reconcile cursors with
    /// `agent_state`: open a sidecar cursor for every Running agent that lacks one (including one
    /// re-opened by `resumedAgentId`), pump it, and drop cursors of agents in any other state.
    /// `main` is the session's main-transcript facts; `alive` is registry liveness.
    /// `budget` is the bytes this call may spend on first-sight windows; it returns what it
    /// spent. Per-file failures are returned as diagnostics, never fatal.
    pub fn refresh(&mut self, main: &Facts, alive: bool, budget: u64) -> (u64, Vec<Problem>) {
        let mut problems = Vec::new();
        let dir_mtime = match FileStamp::of(&self.dir) {
            Ok(s) => s.mtime_ms,
            Err(_) => {
                self.dir_mtime_ms = None;
                self.entries.clear();
                self.open.clear();
                return (0, problems);
            }
        };
        if self.dir_mtime_ms != Some(dir_mtime) {
            self.relist(&mut problems);
            self.dir_mtime_ms = Some(dir_mtime);
        }
        // `stoppedByUser` is merged into the meta file later, in place, which leaves the dir
        // mtime alone. Only a Running agent can still be stopped, so only those metas are
        // re-stated.
        let running: Vec<AgentId> = self.open.keys().cloned().collect();
        for id in &running {
            self.reparse_if_moved(id, &mut problems);
        }

        let mut spent = 0;
        let ids: Vec<AgentId> = self.entries.keys().cloned().collect();
        for id in ids {
            let state = self.state_of(&self.entries[&id].meta, main, alive);
            let has_cursor = self.open.contains_key(&id);
            match (state, has_cursor) {
                (NodeState::Running, true) => {
                    if self.open.get_mut(&id).expect("checked").pump().is_err() {
                        problems.push(Problem::Unreadable);
                        self.drop_cursor(&id);
                    }
                }
                (NodeState::Running, false) => {
                    if budget.saturating_sub(spent) < Window::Sidecar.max_bytes() {
                        problems.push(Problem::Deferred);
                        continue;
                    }
                    let path = self.entries[&id].meta_path.with_extension("").with_extension("jsonl");
                    match Transcript::open(path, Window::Sidecar) {
                        Ok(t) => {
                            spent += t.stamp().len.min(Window::Sidecar.max_bytes());
                            self.open.insert(id, t);
                        }
                        Err(_) => problems.push(Problem::Unreadable),
                    }
                }
                (_, true) => self.drop_cursor(&id),
                (_, false) => {}
            }
        }
        (spent, problems)
    }

    /// Close the sidecar cursor but keep its facts, so the state rule keeps the tail it saw.
    fn drop_cursor(&mut self, id: &AgentId) {
        if let Some(t) = self.open.remove(id) {
            self.entries.get_mut(id).expect("every cursor has an entry").kept = Some(t.facts().clone());
        }
    }

    /// Every known agent with its state and, while running, its sidecar facts (for the now-line).
    /// Order: by id. Depth-2 agents are included; `AgentMeta::parent` places them in the tree.
    pub fn agents<'a>(
        &'a self,
        main: &'a Facts,
        alive: bool,
    ) -> impl Iterator<Item = (&'a AgentMeta, NodeState, Option<&'a Facts>)> + 'a {
        self.entries.values().map(move |e| {
            let sidecar = self.open.get(&e.meta.id).map(Transcript::facts);
            (&e.meta, self.state_of(&e.meta, main, alive), sidecar)
        })
    }

    /// Re-read the directory (and `workflows/<runId>/`), parse new or changed metas, forget
    /// agents whose files are gone.
    fn relist(&mut self, problems: &mut Vec<Problem>) {
        let mut seen: Vec<(AgentId, Option<String>, PathBuf)> = Vec::new();
        for (path, run) in meta_files(&self.dir) {
            let Some(id) = agent_id_of(&path) else { continue };
            seen.push((id, run, path));
        }
        let keep: Vec<&AgentId> = seen.iter().map(|(id, _, _)| id).collect();
        self.entries.retain(|id, _| keep.contains(&id));
        self.open.retain(|id, _| keep.contains(&id));
        for (id, run, path) in seen {
            match self.entries.get_mut(&id) {
                Some(e) => {
                    e.meta_path = path;
                    e.meta.workflow_run = run;
                }
                None => {
                    let mtime = FileStamp::of(&path).map_or(0, |s| s.mtime_ms);
                    match std::fs::read(&path) {
                        Ok(bytes) => match parse_meta(id.clone(), run, &bytes) {
                            Ok(meta) => {
                                self.entries.insert(
                                    id,
                                    Entry { meta_path: path, meta_mtime_ms: mtime, meta, kept: None },
                                );
                            }
                            Err(p) => problems.push(p),
                        },
                        Err(_) => problems.push(Problem::Unreadable),
                    }
                }
            }
        }
    }

    fn reparse_if_moved(&mut self, id: &AgentId, problems: &mut Vec<Problem>) {
        let Some(e) = self.entries.get_mut(id) else { return };
        let Ok(stamp) = FileStamp::of(&e.meta_path) else { return };
        if stamp.mtime_ms == e.meta_mtime_ms {
            return;
        }
        e.meta_mtime_ms = stamp.mtime_ms;
        match std::fs::read(&e.meta_path) {
            Ok(bytes) => match parse_meta(id.clone(), e.meta.workflow_run.clone(), &bytes) {
                Ok(meta) => e.meta = meta,
                Err(p) => problems.push(p),
            },
            Err(_) => problems.push(Problem::Unreadable),
        }
    }

    /// The sidecar facts for `id`: live while it has a cursor, else what was kept.
    fn facts_of(&self, id: &AgentId) -> Option<&Facts> {
        self.open
            .get(id)
            .map(Transcript::facts)
            .or_else(|| self.entries.get(id).and_then(|e| e.kept.as_ref()))
    }

    fn state_of(&self, meta: &AgentMeta, main: &Facts, alive: bool) -> NodeState {
        let (parent_facts, parent_state): (Option<&Facts>, Option<NodeState>) = match &meta.parent {
            None => (Some(main), None),
            Some(pid) => {
                let facts = self.facts_of(pid);
                let state = match self.entries.get(pid) {
                    // A parent that is itself nested is beyond the two levels the tree shows;
                    // treat it as running rather than recurse without bound.
                    Some(p) if p.meta.parent.is_some() => NodeState::Running,
                    Some(p) => self.state_of(&p.meta, main, alive),
                    None => NodeState::Orphaned,
                };
                (facts, Some(state))
            }
        };
        let task = TaskId::from(&meta.id);
        let relaunched = parent_facts.is_some_and(|f| f.open_bg.contains_key(&task));
        let notice = parent_facts.and_then(|f| f.notices.get(&task).copied());
        let side = self.facts_of(&meta.id);
        agent_state(
            meta,
            relaunched,
            notice,
            side.and_then(|f| f.last_activity),
            side.is_some_and(|f| f.tail_looks_finished),
            parent_state,
            parent_facts.map_or(Horizon::default(), |f| f.horizon),
            alive,
        )
    }
}

/// `agent-<id>.meta.json` -> `<id>`.
fn agent_id_of(path: &Path) -> Option<AgentId> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_prefix("agent-")?.strip_suffix(".meta.json")?;
    (!id.is_empty()).then(|| AgentId(id.to_owned()))
}

/// Every meta file in `dir` and one level under `dir/workflows/<runId>/`, with the run id.
fn meta_files(dir: &Path) -> Vec<(PathBuf, Option<String>)> {
    fn metas_in(dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        rd.filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".meta.json")))
            .collect()
    }
    let mut out: Vec<(PathBuf, Option<String>)> = metas_in(dir).into_iter().map(|p| (p, None)).collect();
    if let Ok(runs) = std::fs::read_dir(dir.join("workflows")) {
        for run in runs.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir()) {
            let run_id = run.file_name().and_then(|n| n.to_str()).map(str::to_owned);
            out.extend(metas_in(&run).into_iter().map(|p| (p, run_id.clone())));
        }
    }
    out
}

/// The agent-state rule (GROUNDING 3.9), in one pure function so tests pin it. First match wins:
/// 1. `stopped_by_user` -> Killed (these get no notification, gotcha 24).
/// 2. `relaunched` (the parent's fold has the agent in `open_bg`: a fresh launch, or re-opened
///    by `resumedAgentId`) while the session is alive and the parent is running -> Running.
///    Gated on liveness so a dead session's open launches fall through to Orphaned.
/// 3. a notice with `Some(outcome)` and no sidecar activity after it -> Finished(outcome).
/// 4. `sidecar_looks_finished` (the tail is end-of-turn text or a handback) ->
///    Finished(AwaitingNotice).
/// 5. parent agent not Running (depth 2) -> Orphaned: open children of a finished parent are
///    likely dead.
/// 6. session process dead -> Orphaned.
/// 7. sidecar went quiet before the parent's horizon started and no notice is visible ->
///    Finished(AwaitingNotice): its completion predates our window.
/// 8. otherwise Running.
#[allow(clippy::too_many_arguments)]
pub fn agent_state(
    meta: &AgentMeta,
    relaunched: bool,
    notice: Option<Notice>,
    sidecar_last_activity: Option<At>,
    sidecar_looks_finished: bool,
    parent: Option<NodeState>,
    parent_horizon: Horizon,
    alive: bool,
) -> NodeState {
    if meta.stopped_by_user {
        return NodeState::Killed;
    }
    let parent_running = matches!(parent, None | Some(NodeState::Running));
    if relaunched && alive && parent_running {
        return NodeState::Running;
    }
    if let Some(Notice { at, outcome: Some(outcome) }) = notice
        && sidecar_last_activity.is_none_or(|a| a <= at)
    {
        return NodeState::Finished(outcome);
    }
    if sidecar_looks_finished {
        return NodeState::Finished(TaskOutcome::AwaitingNotice);
    }
    if !parent_running || !alive {
        return NodeState::Orphaned;
    }
    let quiet_before_window = match (sidecar_last_activity, parent_horizon.first_at) {
        (Some(a), Some(h)) => a < h,
        _ => false,
    };
    if notice.is_none() && quiet_before_window {
        return NodeState::Finished(TaskOutcome::AwaitingNotice);
    }
    NodeState::Running
}
