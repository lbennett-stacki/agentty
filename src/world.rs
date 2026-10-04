//! The observer: owns every cursor and cache, and turns "something may have changed" into an
//! up-to-date set of parsed domain values. The only module that does filesystem IO on Claude
//! files (via the parser modules) and the only one that runs `ps` and `tmux list-panes` (via
//! `Host`). Single-threaded; owned by the main loop, so no locks.
//!
//! Change detection is stat-based and is the source of truth: `refresh` stats every tracked
//! file and re-reads only those whose `FileStamp` moved. notify is only a doorbell that makes
//! the next refresh happen sooner; a dropped or coalesced event costs latency, never
//! correctness.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crate::derive::{ExitedView, SessionView};
use crate::jobs::Job;
use crate::model::{At, ConfigDir, Diagnostic, JobId, PaneInfo, Pid, ProcStart, SessionId};
use crate::registry::RegEntry;
use crate::sidecars::{Statusline, Task, Team};
use crate::subagents::Subagents;
use crate::transcript::{FileStamp, Transcript};

/// Filesystem roots. Injected so fixture tests point the whole observer at a fixture tree.
#[derive(Clone, Debug)]
pub struct Roots {
    pub home: PathBuf,
    /// `$CLAUDE_CONFIG_DIR`, if set.
    pub env_config_dir: Option<PathBuf>,
    /// `~/.cache/agentically` (statusline tee lands under `statusline/`).
    pub cache: PathBuf,
}

impl Roots {
    /// `$HOME`, `$CLAUDE_CONFIG_DIR`, `~/.cache/agentically`.
    pub fn from_env() -> anyhow::Result<Roots> {
        todo!()
    }
}

/// The impure edges other than file reads, narrowed to what fixture tests must fake. A
/// `FakeHost` with a fixed clock, a pid table and a pane list makes `World::refresh` a pure
/// function of the fixture tree.
pub trait Host {
    fn now(&self) -> At;
    /// One `ps` call for all candidate pids (`registry::ps_argv` + `registry::parse_ps`). A dead
    /// pid is absent. `Err` means `ps` itself failed: `World` then keeps every candidate alive
    /// and records `DiagSource::Ps / Problem::Unavailable`, so a transient failure never blanks
    /// the list.
    fn proc_starts(&mut self, pids: &[Pid]) -> Result<HashMap<Pid, ProcStart>, HostError>;
    /// One `tmux list-panes -a -F '#{pane_id}\t#{pane_pid}'` (`LIST_PANES_ARGV` + `parse_panes`).
    /// `Err` (no server, not in tmux) yields no verified panes and a `Tmux` diagnostic.
    fn panes(&mut self) -> Result<Vec<PaneInfo>, HostError>;
}

/// A command failure, without its output (which could echo arguments).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError;

/// The real clock, `ps` and `tmux`.
pub struct RealHost;

impl Host for RealHost {
    fn now(&self) -> At {
        todo!()
    }
    fn proc_starts(&mut self, pids: &[Pid]) -> Result<HashMap<Pid, ProcStart>, HostError> {
        todo!("LC_ALL=C TZ=UTC + registry::ps_argv; registry::parse_ps on stdout")
    }
    fn panes(&mut self) -> Result<Vec<PaneInfo>, HostError> {
        todo!()
    }
}

pub const LIST_PANES_ARGV: &[&str] = &["list-panes", "-a", "-F", "#{pane_id}\t#{pane_pid}"];

/// Lenient parse of `list-panes` output; malformed lines are skipped. A pane's absence from this
/// list is the only existence test (gotcha 31).
pub fn parse_panes(stdout: &str) -> Vec<PaneInfo> {
    todo!()
}

/// Why a refresh runs. Decides how much is re-stat'ed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// notify saw a change under a watched dir (`sessions/`, `jobs/`, `tasks/`, statusline
    /// cache). Re-stat registries, jobs, tasks and the statusline cache; run `ps` only for pids
    /// not seen before. Transcripts are not touched.
    Doorbell,
    /// The 1 s tick. Everything `Doorbell` does, plus `ps` for every pid, the tmux pane
    /// snapshot, and a stat-poll of every tracked transcript and sidecar.
    Tick,
}

/// Bytes `refresh` may spend on first-sight transcript windows (the 256 KB -> 1 MB -> 4 MB
/// retries and 64 KB sidecar tails). Files it cannot afford this refresh get a `Deferred`
/// diagnostic and are opened on a later refresh, so a cold start with many large files never
/// stalls one frame for long. Steady-state appends are not budgeted (they are small by
/// construction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadBudget {
    pub bytes_per_refresh: u64,
}

impl Default for ReadBudget {
    fn default() -> ReadBudget {
        ReadBudget { bytes_per_refresh: 4 << 20 }
    }
}

pub struct World {
    roots: Roots,
    host: Box<dyn Host>,
    budget: ReadBudget,
    dirs: Vec<ConfigDir>,
    /// Alive, non-hidden registry entries, in (dir, pid) order.
    live: Vec<Live>,
    /// Registry entries hidden by `spare`/`parkedJobId`; kept only so their jobs are not
    /// mistaken for exited ones.
    hidden_jobs: Vec<(ConfigDir, JobId)>,
    jobs: BTreeMap<(ConfigDir, JobId), (FileStamp, Job)>,
    /// One fold per transcript *file*; two live holders of one sessionId share it.
    transcripts: HashMap<PathBuf, Tracked>,
    /// sessionId -> transcript path. Re-globbed when missing (no transcript before the first
    /// prompt, gotcha 14) at most once per 5 s per session, and whenever the sessionId changes.
    resolved: HashMap<(ConfigDir, SessionId), Resolution>,
    tasks: HashMap<(ConfigDir, SessionId), (Option<FileStamp>, Vec<Task>)>,
    statusline: HashMap<SessionId, Statusline>,
    teams: HashMap<ConfigDir, Vec<Team>>,
    /// cwd -> main repo root. Never invalidated (a cwd does not change repos).
    repos: HashMap<PathBuf, Option<PathBuf>>,
    /// Latest `ps` answer, kept across `Doorbell` refreshes.
    proc_starts: HashMap<Pid, ProcStart>,
    /// Latest tmux pane snapshot.
    panes: Vec<PaneInfo>,
    diagnostics: Vec<Diagnostic>,
}

struct Tracked {
    transcript: Transcript,
    subagents: Subagents,
}

enum Resolution {
    Found(PathBuf),
    Missing { checked: At },
}

/// A live session as observed: the registry entry plus where its transcript is.
#[derive(Clone, Debug)]
pub struct Live {
    pub dir: ConfigDir,
    pub reg: RegEntry,
    pub transcript: Option<PathBuf>,
}

impl World {
    pub fn new(roots: Roots, host: Box<dyn Host>, budget: ReadBudget) -> World {
        todo!()
    }

    /// Bring every cache up to date. Order matters and is fixed here:
    /// 1. discover config dirs; scan registries; `ps` per `Wake` (degrade on failure: keep all
    ///    candidates alive, add a diagnostic); drop hidden entries into `hidden_jobs`;
    /// 2. re-resolve transcripts of sessions whose sessionId changed or was never found;
    /// 3. stat + re-parse changed job state.json files;
    /// 4. on `Tick`: pump every referenced transcript within the read budget, then its subagents
    ///    (which need the main facts); take the tmux pane snapshot;
    /// 5. stat-gated reads of tasks, statusline cache, team configs; repo roots for new cwds;
    /// 6. drop cursors no live session or exited job references (GC).
    /// Never fails as a whole: per-file failures become `Diagnostic`s, replaced each refresh.
    pub fn refresh(&mut self, wake: Wake) {
        todo!()
    }

    /// The host clock, so callers and `World` agree on `now` (fixtures set it).
    pub fn now(&self) -> At {
        self.host.now()
    }

    pub fn sessions(&self) -> impl Iterator<Item = SessionView<'_>> {
        self.live.iter().map(|l| self.view(l))
    }

    fn view<'a>(&'a self, live: &'a Live) -> SessionView<'a> {
        todo!()
    }

    /// bg jobs without a live registry record whose `ended_at` is within 24h of `now`.
    pub fn exited(&self, now: At) -> Vec<ExitedView<'_>> {
        todo!()
    }

    /// Paths for the notify doorbell, each watched recursively: `<dir>/sessions`, `<dir>/jobs`,
    /// `<dir>/tasks` per config dir, plus the statusline cache dir. Transcripts live under
    /// `projects/`, which is deliberately not watched: an active session appends many lines per
    /// second and would ring the bell continuously. They are stat-polled on `Wake::Tick`.
    /// Recomputed after each refresh (config dirs come and go); the caller re-watches on change.
    pub fn watch_roots(&self) -> Vec<PathBuf> {
        todo!()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}
