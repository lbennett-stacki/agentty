//! Boundary for `<dir>/sessions/<pid>.json`, the authoritative "who is alive" record, and for
//! the `ps` output that proves liveness. Yields `RegEntry`; the wire struct never leaves this
//! file. Read-only: never writes, never deletes stale files, never opens `*.key` or the
//! heartbeat.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{At, ConfigDir, JobId, PaneId, Pid, Problem, ProcStart, SessionId, WaitingFor};

/// One registry record, parsed and normalised. Absent keys (gotcha 11) and unknown enum values
/// map to `None` / `Other`, never to a parse failure.
#[derive(Clone, Debug, PartialEq)]
pub struct RegEntry {
    pub pid: Pid,
    pub proc_start: ProcStart,
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub kind: ProcKind,
    pub entrypoint: String,
    pub status: RegStatus,
    /// `statusUpdatedAt`: status writes are deduplicated, so this is "in this status since".
    pub status_since: At,
    pub name: Option<Name>,
    /// `%N` from the `tmux` field; None outside tmux and on every bg session.
    pub pane: Option<PaneId>,
    pub remote_control: bool,
    pub job: Option<JobId>,
    /// `spare: true` or `parkedJobId` set. Kept (not filtered here) so `World` can still join a
    /// parked interactive record to its job; `derive` never renders a hidden entry.
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcKind {
    Interactive,
    Bg,
    Other(String),
}

/// The four registry states. Precedence inside Claude is waiting > busy > shell > idle; this
/// enum just carries the value. `waitingFor` is folded into `Waiting`, so waiting-without-reason
/// (`Unspecified`) and reason-without-waiting are unrepresentable. `Other` (a future state) is
/// treated as Idle by derive and reported as a `Diagnostic`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegStatus {
    Busy,
    Shell,
    Idle,
    Waiting(WaitingFor),
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub value: String,
    pub source: NameSource,
}

/// `nameSource`. Only `User` and `Auto` outrank ai-title in the title precedence; `Derived`
/// (`<cwd-basename>-<hash>`) is a last resort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameSource {
    User,
    Auto,
    Derived,
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireReg {
    pid: Option<u32>,
    proc_start: Option<String>,
    session_id: Option<String>,
    cwd: Option<PathBuf>,
    kind: Option<String>,
    entrypoint: Option<String>,
    status: Option<String>,
    waiting_for: Option<String>,
    status_updated_at: Option<i64>,
    name: Option<String>,
    name_source: Option<String>,
    tmux: Option<String>,
    bridge_session_id: Option<String>,
    job_id: Option<String>,
    spare: Option<bool>,
    parked_job_id: Option<String>,
}

/// Pure parse. `Err(MissingField)` when an identity field (pid, procStart, sessionId) is absent:
/// without them a row cannot be keyed, so it is dropped and reported.
pub fn parse(bytes: &[u8]) -> Result<RegEntry, Problem> {
    todo!("serde_json::from_slice::<WireReg>, then map strings to enums with Other fallbacks")
}

/// Read every `^\d+\.json$` in `<dir>/sessions/` (never `*.key`, never the heartbeat). The file is
/// rewritten without atomic rename (gotcha 8): on a parse failure, retry once after 25 ms, then
/// skip the file for this refresh. Returns the parsed entries and one problem per skipped file.
pub fn scan(dir: &ConfigDir) -> (Vec<RegEntry>, Vec<(PathBuf, Problem)>) {
    todo!()
}

/// Path of a config dir's sessions dir; also a notify watch target.
pub fn sessions_dir(dir: &ConfigDir) -> PathBuf {
    dir.0.join("sessions")
}

/// `LC_ALL=C TZ=UTC ps -o pid=,lstart= -p a,b,c`: one call per refresh for every candidate pid.
/// The env is set by the caller; this is only the argv.
pub fn ps_argv(pids: &[Pid]) -> Vec<String> {
    todo!()
}

/// Parse `ps` output into pid -> trimmed start time (gotcha 7: trailing spaces, double-space
/// day). A pid absent from the output is dead. Malformed lines are skipped.
pub fn parse_ps(stdout: &str) -> HashMap<Pid, ProcStart> {
    todo!()
}

/// Every config dir: `~/.claude`, each `~/.claude-*` with a `sessions/` dir, and
/// `$CLAUDE_CONFIG_DIR` if set. Deduplicated by canonical path. Re-run on every refresh (a new
/// dir appears when the user first launches with a new CLAUDE_CONFIG_DIR).
pub fn discover_config_dirs(home: &Path, env_override: Option<&Path>) -> Vec<ConfigDir> {
    todo!()
}
