//! Boundary for `projects/<slug>/<sid>/subagents/` (flat at every depth; workflows under
//! `subagents/workflows/<runId>/`). Yields `AgentMeta` plus one sidecar `Transcript` per agent
//! that may still be running.
//!
//! Cost control: the dir listing is redone only when the dir's mtime changes (a new agent adds
//! files). Each agent jsonl is pumped (one stat) only while the agent is `Running`; any other
//! state drops its cursor. Sessions with 100+ historical agents cost ~0 per refresh.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{AgentId, At, NodeState, Problem, SafeText, ToolUseId};
use crate::transcript::{Facts, Horizon, Notice, Transcript};

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
    todo!()
}

/// Cached view of one session's subagents dir.
pub struct Subagents {
    dir: PathBuf,
    dir_mtime_ms: Option<i64>,
    /// meta.json is rewritten (stoppedByUser merged in later), so each meta keeps its mtime and
    /// is re-parsed when it changes.
    metas: BTreeMap<AgentId, (i64, AgentMeta)>,
    /// Sidecar folds for Running agents only.
    open: BTreeMap<AgentId, Transcript>,
}

impl Subagents {
    /// `<transcript dir>/<sid>/subagents`. The dir may not exist yet; that is an empty set.
    pub fn new(main_transcript: &Path) -> Subagents {
        todo!()
    }

    /// Re-list if the dir changed, re-parse changed metas, then reconcile cursors with
    /// `agent_state`: open a sidecar cursor for every Running agent that lacks one (including one
    /// re-opened by `resumedAgentId`), pump it, and drop cursors of agents in any other state.
    /// `main` is the session's main-transcript facts; `alive` is registry liveness.
    /// `budget` is the bytes this call may spend on first-sight windows; it returns what it
    /// spent. Per-file failures are returned as diagnostics, never fatal.
    pub fn refresh(&mut self, main: &Facts, alive: bool, budget: u64) -> (u64, Vec<Problem>) {
        todo!()
    }

    /// Every known agent with its state and, while running, its sidecar facts (for the now-line).
    /// Order: by id. Depth-2 agents are included; `AgentMeta::parent` places them in the tree.
    pub fn agents<'a>(
        &'a self,
        main: &'a Facts,
        alive: bool,
    ) -> impl Iterator<Item = (&'a AgentMeta, NodeState, Option<&'a Facts>)> + 'a {
        self.metas.values().map(move |(_, m)| {
            let sidecar = self.open.get(&m.id).map(Transcript::facts);
            (m, self.state_of(m, main, alive), sidecar)
        })
    }

    fn state_of(&self, meta: &AgentMeta, main: &Facts, alive: bool) -> NodeState {
        todo!("gather the notice from main (depth 1) or the parent's sidecar (depth 2), then agent_state")
    }
}

/// The agent-state rule (GROUNDING 3.9), in one pure function so tests pin it. First match wins:
/// 1. `stopped_by_user` -> Killed (these get no notification, gotcha 24).
/// 2. the parent shows the agent open again (`relaunched`: in `open_bg` after a resume) -> Running.
/// 3. a notice with `Some(outcome)` newer than the sidecar's last activity -> Finished(outcome).
/// 4. `sidecar_looks_finished` and no newer activity -> Finished(AwaitingNotice).
/// 5. parent agent not Running (depth 2) -> Orphaned: open children of a finished parent are
///    likely dead.
/// 6. session process dead -> Orphaned.
/// 7. sidecar went quiet before the parent's horizon started and no notice is visible ->
///    Finished(AwaitingNotice): its completion predates our window.
/// 8. otherwise Running.
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
    todo!()
}
