//! Boundaries for the small per-session side files: the task list, the optional statusline
//! cache, team configs, and git repo roots for auto-grouping. Each is one pure `parse_*` plus a
//! stamp-gated reader in `World`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{At, ConfigDir, PaneId, Problem, SafeText, SessionId};

// --- tasks/<listId>/<n>.json --------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub id: u32,
    pub subject: SafeText,
    pub status: TaskStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Other(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTask {
    id: Option<serde_json::Value>,
    subject: Option<String>,
    status: Option<String>,
}

pub fn parse_task(bytes: &[u8]) -> Result<Task, Problem> {
    todo!()
}

/// `<dir>/tasks/<sid>/`. Only `^\d+\.json$` files; `.lock`, `.highwatermark` ignored. An empty or
/// missing dir means "nothing open" (lists self-erase when complete, gotcha 27).
pub fn task_dir(dir: &ConfigDir, sid: &SessionId) -> PathBuf {
    dir.0.join("tasks").join(&sid.0)
}

/// `<dir>/tasks/`; a notify watch target.
pub fn tasks_root(dir: &ConfigDir) -> PathBuf {
    dir.0.join("tasks")
}

// --- ~/.cache/agentically/statusline/<sid>.json (opt-in tee) ------------------------------------

/// Exact values from the user's statusline, when they installed the tee. The only source of a
/// context *percentage* and of a live cost.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Statusline {
    pub ctx_used_pct: Option<f64>,
    pub window: Option<u64>,
    pub cost_usd: Option<f64>,
    /// File mtime. Used only if newer than the transcript's latest usage (else stale).
    pub at: At,
}

#[derive(Deserialize)]
struct WireStatusline {
    context_window: Option<WireCtxWindow>,
    cost: Option<WireCost>,
}

#[derive(Deserialize)]
struct WireCtxWindow {
    used_percentage: Option<f64>,
    context_window_size: Option<u64>,
}

#[derive(Deserialize)]
struct WireCost {
    total_cost_usd: Option<f64>,
}

pub fn parse_statusline(bytes: &[u8], mtime: At) -> Result<Statusline, Problem> {
    todo!()
}

/// `<cache>/statusline/`; a notify watch target.
pub fn statusline_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("statusline")
}

pub fn statusline_path(cache_root: &Path, sid: &SessionId) -> PathBuf {
    statusline_dir(cache_root).join(format!("{}.json", sid.0))
}

// --- <dir>/teams/<team>/config.json -----------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Team {
    pub name: String,
    pub lead: Option<SessionId>,
    pub members: Vec<TeamMember>,
}

/// Split-pane members never write a registry record; they appear only here.
#[derive(Clone, Debug, PartialEq)]
pub struct TeamMember {
    pub name: String,
    pub pane: Option<PaneId>,
    pub active: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTeam {
    name: Option<String>,
    lead_session_id: Option<String>,
    #[serde(default)]
    members: Vec<WireMember>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMember {
    name: Option<String>,
    tmux_pane_id: Option<String>,
    is_active: Option<bool>,
}

pub fn parse_team(bytes: &[u8]) -> Result<Team, Problem> {
    todo!()
}

// --- git repo root for auto-grouping ----------------------------------------------------------

/// The main worktree root containing `cwd`, by walking up to the first `.git`. A `.git` *file*
/// (`gitdir: <main>/.git/worktrees/<name>`) is a linked worktree: fold it into `<main>`. No
/// subprocess; `World` caches the answer per cwd for the life of the process.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    todo!()
}
