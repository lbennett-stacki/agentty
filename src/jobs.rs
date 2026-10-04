//! Boundary for `<dir>/jobs/<jobId>/state.json` (bg sessions).
//!
//! SECURITY: the file holds `providerEnv` (live credentials). `WireJob` has no such field, so
//! serde skips it as an ignored value and it is never materialised. Parse errors are reported as
//! a `Problem` category only, never via `serde_json::Error`'s Display, which can quote values.

use std::path::PathBuf;

use serde::Deserialize;

use crate::model::{At, ConfigDir, JobId, Problem, SafeText, SessionId};

/// A bg job, parsed. Classifier fields (`state`, `tempo`, `detail`, `needs`) are LLM verdicts that
/// go stale (gotcha 4); derive consults them only where the registry is silent.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: JobId,
    pub session_id: Option<SessionId>,
    pub state: JobState,
    pub tempo: Option<Tempo>,
    pub detail: Option<SafeText>,
    pub needs: Option<SafeText>,
    pub fan: Vec<FanItem>,
    /// `linkScanPath`: the exact transcript path. None before the first prompt.
    pub transcript: Option<PathBuf>,
    pub worktree_path: Option<PathBuf>,
    pub worktree_branch: Option<String>,
    pub name: Option<String>,
    pub remote_control: bool,
    /// `children[kind==pr]` numbers. Other children (frames) are dropped.
    pub prs: Vec<u32>,
    pub updated_at: Option<At>,
    /// `lastTerminalAt`, else `updatedAt`. Drives the 24h Exited window.
    pub ended_at: Option<At>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Starting,
    Running,
    Working,
    Blocked,
    Done,
    Failed,
    Stopped,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tempo {
    Active,
    Idle,
    Blocked,
    Other(String),
}

/// A `fan[]` entry. For bg rows this is the authoritative live roster of background work.
#[derive(Clone, Debug, PartialEq)]
pub struct FanItem {
    pub id: String,
    pub kind: FanKind,
    /// Already redacted. For `Shell` the raw label is the command line (may hold secrets), so the
    /// parser keeps only the first word after stripping env assignments, then redacts.
    pub label: SafeText,
    pub started_at: Option<At>,
    /// None means open.
    pub done_at: Option<At>,
    pub failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FanKind {
    Agent,
    Workflow,
    Shell,
    Monitor,
    Mcp,
    Todo,
    Other(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireJob {
    session_id: Option<String>,
    state: Option<String>,
    tempo: Option<String>,
    detail: Option<String>,
    needs: Option<serde_json::Value>,
    #[serde(default)]
    fan: Vec<WireFan>,
    link_scan_path: Option<PathBuf>,
    worktree_path: Option<PathBuf>,
    worktree_branch: Option<String>,
    name: Option<String>,
    bridge_session_id: Option<String>,
    #[serde(default)]
    children: Vec<WireChild>,
    updated_at: Option<String>,
    last_terminal_at: Option<String>,
    // providerEnv: deliberately absent. Do not add it.
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFan {
    id: Option<String>,
    kind: Option<String>,
    label: Option<String>,
    started_at: Option<i64>,
    done_at: Option<i64>,
    failed: Option<bool>,
}

#[derive(Deserialize)]
struct WireChild {
    kind: Option<String>,
    title: Option<String>,
    href: Option<String>,
}

/// Pure parse of one state.json. The JobId comes from the dir name, not the file.
pub fn parse(id: JobId, bytes: &[u8]) -> Result<Job, Problem> {
    todo!()
}

/// `<dir>/jobs/`; a notify watch target (recursive, so state.json rewrites ring the bell).
pub fn jobs_dir(dir: &ConfigDir) -> PathBuf {
    dir.0.join("jobs")
}

impl Job {
    /// Open fan items by kind, for Parked counts and tree leaves. Todos are excluded (they are
    /// tasks, shown in the detail pane, not background work).
    pub fn open_work(&self) -> impl Iterator<Item = &FanItem> {
        self.fan.iter().filter(|f| f.done_at.is_none() && f.kind != FanKind::Todo)
    }
}
