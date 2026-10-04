//! Pure derivation: observed facts -> `Snapshot`. No IO, no clock (time is a parameter), no
//! user state (labels and groups are applied by `ui::layout`). Every product decision about
//! status, title, now-line, tree and auto-grouping lives here, as small functions over
//! `SessionView`, so fixture tests pin each one. This module never imports `world`: the
//! observer builds `SessionView`s and hands them in.

use std::path::Path;

use crate::jobs::Job;
use crate::model::{
    At, AutoGroup, ConfigDir, Detail, Diagnostic, JobOutcome, Meta, Need, OpenTarget, PaneInfo,
    Parked, Row, SafeText, Snapshot, Status, Tree, WaitingFor,
};
use crate::registry::RegEntry;
use crate::sidecars::{Statusline, Task, Team};
use crate::subagents::Subagents;
use crate::transcript::Facts;

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
    todo!()
}

pub fn live_row(v: SessionView<'_>, now: At) -> Row {
    todo!()
}

pub fn exited_row(v: ExitedView<'_>) -> Row {
    todo!()
}

/// THE status table (GROUNDING 3.3). Returns the status and its `since`.
///
/// ```text
/// registry      transcript / job                              -> Status                   since
/// waiting(w)    -                                             -> NeedsYou(need_of(w))     statusUpdatedAt
/// busy          turn Open                                     -> Working                  max(statusUpdatedAt, turn.since)
/// busy          turn Closed/Interrupted/Unknown                -> Parked(parked())         turn end, else statusUpdatedAt
/// shell         -                                             -> Parked(parked())         statusUpdatedAt
/// idle          bg: tempo Blocked && needs                     -> NeedsYou(JobQuestion)    statusUpdatedAt
/// idle          wakeup in the future                           -> Parked(wakeup)           statusUpdatedAt
/// idle          -                                             -> Idle{asked_question}     statusUpdatedAt
/// Other         -                                             -> Idle{false} (+ diagnostic)
/// ```
/// `asked_question` = last turn Closed and `last_text.is_question`. `job.state` is never
/// consulted for liveness or Parked-vs-Idle (it says `done` while work runs, gotcha 4).
pub fn status(v: &SessionView<'_>, now: At) -> (Status, At) {
    todo!()
}

/// `WaitingFor::Permission` -> `Need::Permission { tool }` where `tool` is the name of
/// `facts.last_tool` if it is unanswered; every other reason -> `Need::Waiting(w)`.
pub fn need_of(w: &WaitingFor, facts: Option<&Facts>) -> Need {
    todo!()
}

/// What a parked session waits on. Source precedence: bg `fan` (authoritative roster) > the
/// transcript fold's `open_bg` > `turn.pending_agents`. Each count is `Exact` when the source
/// is complete (fan, or fold with `horizon.from_start`), else `AtLeast`. Agents =
/// max(fold agents, live tree agents, pending_agents), since pending_agents also counts depth-2
/// agents the fold cannot see.
///
/// Registry fallbacks, applied when no source shows anything, so `Parked::new` always succeeds:
/// - `shell` -> `shells: AtLeast(1)` (the registry guarantees a live local_bash or monitor);
/// - `busy` with a closed turn -> `agents: AtLeast(1)` (something non-terminal keeps it busy).
pub fn parked(v: &SessionView<'_>, registry_says_shell: bool, now: At) -> Parked {
    todo!()
}

/// Title precedence (SPEC; GROUNDING 3.4), user label excluded (applied by `ui::layout`):
/// custom-title > agent-name > registry name (source User|Auto) > ai-title > job name >
/// registry name (any source) > short sessionId.
pub fn title(v: &SessionView<'_>) -> SafeText {
    todo!()
}

/// The second line, specific to the status (GROUNDING 3.5):
/// - NeedsYou(Permission{tool}): "permission prompt · <tool>: <summary of the dangling tool_use>"
/// - NeedsYou(Waiting(w)): w.label()
/// - NeedsYou(JobQuestion): the question text
/// - Working: "<tool>: <summary>" of last_tool if unanswered, else last_text first line
/// - Parked: "waiting on 3 agents, 1+ shell, 1 monitor · wakes 14:05" (`describe_parked`)
/// - Idle: away_summary ?? last_text ?? last_prompt
/// - bg rows: job.detail replaces Working/Idle text when job.updated_at is within 2 minutes.
pub fn now_line(status: &Status, v: &SessionView<'_>, now: At) -> SafeText {
    todo!()
}

/// Expandable tree (depth 2): agents from `Subagents::agents` placed by `parent`; open shells and
/// monitors as leaves (from `fan` for bg rows, else from `open_bg`); a pending wakeup as a leaf.
/// Finished agents at each level collapse into `Tree::finished`; Running, Killed and Orphaned
/// stay as nodes with their state.
pub fn tree(v: &SessionView<'_>) -> Tree {
    todo!()
}

/// Right-aligned meta. ctx: statusline percent if its mtime is newer than the latest usage,
/// else absolute tokens from `facts.usage`. cost: statusline only (Live); never estimated.
/// branch: facts.branch ?? worktree branch ?? job.worktree_branch. prs: facts.prs ∪ job.prs.
pub fn meta(v: &SessionView<'_>) -> Meta {
    todo!()
}

pub fn detail(v: &SessionView<'_>) -> Detail {
    todo!()
}

/// Enter's behaviour, decided once per row:
/// interactive + pane present in `v.panes` -> Pane; bg (alive or exited) + job -> Attach;
/// otherwise Unreachable with "running in <entrypoint>" (VS Code, Desktop, a terminal outside
/// tmux) or "pane gone".
pub fn open_target(v: &SessionView<'_>, title: &SafeText) -> OpenTarget {
    todo!()
}

/// Automatic group: team (session is a lead or member) > repo root > Elsewhere. The user's
/// assignment, applied in `ui::layout`, overrides this.
pub fn auto_group(v: &SessionView<'_>) -> AutoGroup {
    todo!()
}

/// Render-ready "3 agents, 1+ shell, 1 monitor". Zero counts omitted; `AtLeast(n)` renders `n+`.
pub fn describe_parked(p: &Parked) -> String {
    todo!()
}

/// Map a job's terminal classifier state to the Exited outcome. Non-terminal states on a dead
/// process are Unknown (it crashed or was killed mid-flight).
pub fn job_outcome(job: &Job) -> JobOutcome {
    todo!()
}
