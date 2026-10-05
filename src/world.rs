use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::derive::{ExitedView, SessionView};
use crate::jobs::{self, Job};
use crate::model::{
    At, ConfigDir, DiagSource, Diagnostic, JobId, PaneId, PaneInfo, Pid, Problem, ProcStart,
    SessionId,
};
use crate::registry::{self, RegEntry};
use crate::sidecars::{self, Statusline, Task, Team};
use crate::subagents::Subagents;
use crate::transcript::{FileStamp, Transcript, Window};

const EXITED_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const REGLOB_MS: i64 = 5_000;

#[derive(Clone, Debug)]
pub struct Roots {
    pub home: PathBuf,
    pub env_config_dir: Option<PathBuf>,
    pub cache: PathBuf,
}

impl Roots {
    pub fn from_env() -> anyhow::Result<Roots> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        let env_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Ok(Roots {
            cache: home.join(".cache").join("agentty"),
            home,
            env_config_dir,
        })
    }
}

pub trait Host {
    fn now(&self) -> At;
    fn proc_starts(&mut self, pids: &[Pid]) -> Result<HashMap<Pid, ProcStart>, HostError>;
    fn panes(&mut self) -> Result<Vec<PaneInfo>, HostError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError;

pub struct RealHost;

impl Host for RealHost {
    fn now(&self) -> At {
        At::now()
    }

    fn proc_starts(&mut self, pids: &[Pid]) -> Result<HashMap<Pid, ProcStart>, HostError> {
        let out = Command::new("ps")
            .args(registry::ps_argv(pids))
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
            .map_err(|_| HostError)?;
        // `ps -p` exits 1 with no output at all when every listed pid is dead. That is an answer;
        // anything on stderr or another code is a failure.
        let answered =
            out.status.success() || (out.status.code() == Some(1) && out.stderr.is_empty());
        if !answered {
            return Err(HostError);
        }
        Ok(registry::parse_ps(&String::from_utf8_lossy(&out.stdout)))
    }

    fn panes(&mut self) -> Result<Vec<PaneInfo>, HostError> {
        let out = Command::new("tmux")
            .args(LIST_PANES_ARGV)
            .output()
            .map_err(|_| HostError)?;
        if !out.status.success() {
            return Err(HostError);
        }
        Ok(parse_panes(&String::from_utf8_lossy(&out.stdout)))
    }
}

pub const LIST_PANES_ARGV: &[&str] = &["list-panes", "-a", "-F", "#{pane_id}\t#{pane_pid}"];

pub fn parse_panes(stdout: &str) -> Vec<PaneInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let (id, pid) = line.split_once('\t')?;
            Some(PaneInfo {
                id: PaneId::parse(id)?,
                pid: Pid(pid.trim().parse().ok()?),
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    Doorbell,
    Tick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadBudget {
    pub bytes_per_refresh: u64,
}

impl Default for ReadBudget {
    fn default() -> ReadBudget {
        ReadBudget {
            bytes_per_refresh: 4 << 20,
        }
    }
}

pub struct World {
    roots: Roots,
    host: Box<dyn Host>,
    budget: ReadBudget,
    dirs: Vec<ConfigDir>,
    live: Vec<Live>,
    live_jobs: HashSet<(ConfigDir, JobId)>,
    jobs: BTreeMap<(ConfigDir, JobId), (FileStamp, Job)>,
    transcripts: BTreeMap<PathBuf, Tracked>,
    resolved: HashMap<(ConfigDir, SessionId), Resolution>,
    tasks: HashMap<(ConfigDir, SessionId), (Vec<FileStamp>, Vec<Task>)>,
    statusline: HashMap<SessionId, Statusline>,
    teams: BTreeMap<PathBuf, (FileStamp, Team)>,
    repos: HashMap<PathBuf, Option<PathBuf>>,
    proc_starts: HashMap<Pid, ProcStart>,
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

#[derive(Clone, Debug)]
pub struct Live {
    pub dir: ConfigDir,
    pub reg: RegEntry,
    pub transcript: Option<PathBuf>,
}

impl World {
    pub fn new(roots: Roots, host: Box<dyn Host>, budget: ReadBudget) -> World {
        World {
            roots,
            host,
            budget,
            dirs: Vec::new(),
            live: Vec::new(),
            live_jobs: HashSet::new(),
            jobs: BTreeMap::new(),
            transcripts: BTreeMap::new(),
            resolved: HashMap::new(),
            tasks: HashMap::new(),
            statusline: HashMap::new(),
            teams: BTreeMap::new(),
            repos: HashMap::new(),
            proc_starts: HashMap::new(),
            panes: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    pub fn refresh(&mut self, wake: Wake) {
        let tick = wake == Wake::Tick;
        self.diagnostics.retain(|d| {
            !tick
                && matches!(
                    d.source,
                    DiagSource::Transcript
                        | DiagSource::Subagent
                        | DiagSource::Tmux
                        | DiagSource::Ps
                )
        });
        let now = self.host.now();
        self.dirs =
            registry::discover_config_dirs(&self.roots.home, self.roots.env_config_dir.as_deref());
        self.observe_processes(tick);
        self.refresh_jobs();
        self.resolve_transcripts(now);
        if tick {
            self.pump_transcripts(now);
            match self.host.panes() {
                Ok(panes) => self.panes = panes,
                Err(HostError) => {
                    self.panes.clear();
                    self.note(DiagSource::Tmux, Problem::Unavailable, "tmux".into());
                }
            }
        }
        self.refresh_tasks();
        self.refresh_statusline();
        self.refresh_teams();
        self.refresh_repos(now);
        let mut seen = Vec::new();
        self.diagnostics.retain(|d| {
            let first = !seen.contains(d);
            seen.push(d.clone());
            first
        });
    }

    fn note(&mut self, source: DiagSource, problem: Problem, path: String) {
        self.diagnostics.push(Diagnostic {
            source,
            problem,
            path,
        });
    }

    fn observe_processes(&mut self, tick: bool) {
        let mut candidates = Vec::new();
        for dir in &self.dirs {
            let (entries, problems) = registry::scan(dir);
            candidates.extend(entries.into_iter().map(|e| (dir.clone(), e)));
            for (path, problem) in problems {
                self.diagnostics
                    .push(diag(&self.roots.home, DiagSource::Registry, problem, &path));
            }
        }

        let mut ask: Vec<Pid> = candidates
            .iter()
            .filter(|(_, r)| tick || self.proc_starts.get(&r.pid) != Some(&r.proc_start))
            .map(|(_, r)| r.pid)
            .collect();
        ask.sort();
        ask.dedup();
        let mut unverified = HashSet::new();
        if !ask.is_empty() {
            match self.host.proc_starts(&ask) {
                Ok(found) => {
                    for pid in &ask {
                        self.proc_starts.remove(pid);
                    }
                    self.proc_starts.extend(found);
                }
                Err(HostError) => {
                    unverified.extend(ask);
                    self.note(DiagSource::Ps, Problem::Unavailable, "ps".into());
                }
            }
        }
        let candidate_pids: HashSet<Pid> = candidates.iter().map(|(_, r)| r.pid).collect();
        self.proc_starts
            .retain(|pid, _| candidate_pids.contains(pid));

        self.live.clear();
        self.live_jobs.clear();
        for (dir, reg) in candidates {
            let alive = unverified.contains(&reg.pid)
                || self.proc_starts.get(&reg.pid) == Some(&reg.proc_start);
            if !alive {
                continue;
            }
            if let Some(job) = &reg.job {
                self.live_jobs.insert((dir.clone(), job.clone()));
            }
            if !reg.hidden {
                self.live.push(Live {
                    dir,
                    reg,
                    transcript: None,
                });
            }
        }
        self.live
            .sort_by(|a, b| (&a.dir, a.reg.pid).cmp(&(&b.dir, b.reg.pid)));
    }

    fn refresh_jobs(&mut self) {
        let mut next = BTreeMap::new();
        for dir in &self.dirs {
            let Ok(listing) = std::fs::read_dir(jobs::jobs_dir(dir)) else {
                continue;
            };
            for entry in listing.flatten() {
                let Some(id) = entry.file_name().to_str().and_then(JobId::parse) else {
                    continue;
                };
                let path = entry.path().join("state.json");
                let Ok(now_stamp) = FileStamp::of(&path) else {
                    continue;
                };
                let key = (dir.clone(), id);
                let previous = self.jobs.remove(&key);
                if let Some(prev) = previous.as_ref().filter(|(s, _)| *s == now_stamp) {
                    next.insert(key, prev.clone());
                    continue;
                }
                let parsed = std::fs::read(&path)
                    .map_err(|_| Problem::Unreadable)
                    .and_then(|bytes| jobs::parse(key.1.clone(), &bytes));
                match parsed {
                    Ok(job) => {
                        next.insert(key, (now_stamp, job));
                    }
                    Err(problem) => {
                        self.diagnostics.push(diag(
                            &self.roots.home,
                            DiagSource::Job,
                            problem,
                            &path,
                        ));
                        if let Some(prev) = previous {
                            next.insert(key, prev);
                        }
                    }
                }
            }
        }
        self.jobs = next;
    }

    fn job(&self, dir: &ConfigDir, reg: &RegEntry) -> Option<&Job> {
        let id = reg.job.as_ref()?;
        self.jobs.get(&(dir.clone(), id.clone())).map(|(_, j)| j)
    }

    fn resolve_transcripts(&mut self, now: At) {
        let mut referenced = HashSet::new();
        for i in 0..self.live.len() {
            let Live { dir, reg, .. } = &self.live[i];
            if let Some(p) = self.job(dir, reg).and_then(|j| j.transcript.clone()) {
                self.live[i].transcript = Some(p);
                continue;
            }
            let key = (dir.clone(), reg.session_id.clone());
            let path = match self.resolved.get(&key) {
                Some(Resolution::Found(p)) if p.is_file() => Some(p.clone()),
                Some(Resolution::Missing { checked }) if now.millis_since(*checked) < REGLOB_MS => {
                    None
                }
                _ => {
                    let found = glob_transcript(dir, &reg.session_id);
                    let r = match &found {
                        Some(p) => Resolution::Found(p.clone()),
                        None => Resolution::Missing { checked: now },
                    };
                    self.resolved.insert(key.clone(), r);
                    found
                }
            };
            referenced.insert(key);
            self.live[i].transcript = path;
        }
        self.resolved.retain(|k, _| referenced.contains(k));
    }

    fn exited_jobs(&self, now: At) -> impl Iterator<Item = (&ConfigDir, &Job)> {
        self.jobs.iter().filter_map(move |(key, (_, job))| {
            let recent = job
                .ended_at
                .is_some_and(|e| now.millis_since(e) <= EXITED_WINDOW_MS);
            (recent && !self.live_jobs.contains(key)).then_some((&key.0, job))
        })
    }

    fn pump_transcripts(&mut self, now: At) {
        let mut refs: BTreeMap<PathBuf, bool> = BTreeMap::new();
        for p in self.live.iter().filter_map(|l| l.transcript.as_ref()) {
            refs.insert(p.clone(), true);
        }
        for p in self
            .exited_jobs(now)
            .filter_map(|(_, j)| j.transcript.as_ref())
        {
            refs.entry(p.clone()).or_insert(false);
        }
        self.transcripts.retain(|p, _| refs.contains_key(p));

        let budget = self.budget.bytes_per_refresh;
        let mut spent = 0u64;
        let home = &self.roots.home;
        for (path, alive) in refs {
            let tracked = match self.transcripts.entry(path.clone()) {
                Entry::Occupied(mut e) => {
                    if e.get_mut().transcript.pump().is_err() {
                        e.remove();
                        self.diagnostics.push(diag(
                            home,
                            DiagSource::Transcript,
                            Problem::Unreadable,
                            &path,
                        ));
                        continue;
                    }
                    e.into_mut()
                }
                Entry::Vacant(v) => {
                    if spent >= budget {
                        self.diagnostics.push(diag(
                            home,
                            DiagSource::Transcript,
                            Problem::Deferred,
                            &path,
                        ));
                        continue;
                    }
                    match Transcript::open(path.clone(), Window::Main) {
                        Ok(transcript) => {
                            spent += transcript.window_bytes();
                            v.insert(Tracked {
                                subagents: Subagents::new(&path),
                                transcript,
                            })
                        }
                        Err(_) => {
                            self.diagnostics.push(diag(
                                home,
                                DiagSource::Transcript,
                                Problem::Unreadable,
                                &path,
                            ));
                            continue;
                        }
                    }
                }
            };
            let Tracked {
                transcript,
                subagents,
            } = tracked;
            let (used, problems) =
                subagents.refresh(transcript.facts(), alive, budget.saturating_sub(spent));
            spent += used;
            let dir = path.with_extension("").join("subagents");
            self.diagnostics.extend(
                problems
                    .into_iter()
                    .map(|p| diag(home, DiagSource::Subagent, p, &dir)),
            );
        }
    }

    fn refresh_tasks(&mut self) {
        let mut next = HashMap::new();
        for l in &self.live {
            let key = (l.dir.clone(), l.reg.session_id.clone());
            if next.contains_key(&key) {
                continue;
            }
            let files = task_files(&sidecars::task_dir(&l.dir, &l.reg.session_id));
            let stamps: Vec<FileStamp> = files.iter().map(|(_, s)| *s).collect();
            let entry = match self.tasks.remove(&key) {
                Some((old, tasks)) if old == stamps => (old, tasks),
                _ => {
                    let mut tasks = Vec::new();
                    for (path, _) in &files {
                        let parsed = std::fs::read(path)
                            .map_err(|_| Problem::Unreadable)
                            .and_then(|b| sidecars::parse_task(&b));
                        match parsed {
                            Ok(t) => tasks.push(t),
                            Err(Problem::Unreadable) if !path.exists() => {}
                            Err(p) => self.diagnostics.push(diag(
                                &self.roots.home,
                                DiagSource::Tasks,
                                p,
                                path,
                            )),
                        }
                    }
                    tasks.sort_by_key(|t| t.id);
                    (stamps, tasks)
                }
            };
            next.insert(key, entry);
        }
        self.tasks = next;
    }

    fn refresh_statusline(&mut self) {
        let mut next = HashMap::new();
        for sid in self.live.iter().map(|l| &l.reg.session_id) {
            let path = sidecars::statusline_path(&self.roots.cache, sid);
            let Ok(FileStamp { mtime_ms, .. }) = FileStamp::of(&path) else {
                continue;
            };
            match self.statusline.remove(sid) {
                Some(s) if s.at == At(mtime_ms) => {
                    next.insert(sid.clone(), s);
                }
                _ => {
                    let parsed = std::fs::read(&path)
                        .map_err(|_| Problem::Unreadable)
                        .and_then(|b| sidecars::parse_statusline(&b, At(mtime_ms)));
                    match parsed {
                        Ok(s) => {
                            next.insert(sid.clone(), s);
                        }
                        Err(p) => self.diagnostics.push(diag(
                            &self.roots.home,
                            DiagSource::Statusline,
                            p,
                            &path,
                        )),
                    }
                }
            }
        }
        self.statusline = next;
    }

    fn refresh_teams(&mut self) {
        let mut next = BTreeMap::new();
        for dir in &self.dirs {
            let Ok(listing) = std::fs::read_dir(dir.0.join("teams")) else {
                continue;
            };
            for path in listing.flatten().map(|e| e.path().join("config.json")) {
                let Ok(now_stamp) = FileStamp::of(&path) else {
                    continue;
                };
                match self.teams.remove(&path) {
                    Some(prev) if prev.0 == now_stamp => {
                        next.insert(path, prev);
                    }
                    _ => {
                        let parsed = std::fs::read(&path)
                            .map_err(|_| Problem::Unreadable)
                            .and_then(|b| sidecars::parse_team(&b));
                        match parsed {
                            Ok(team) => {
                                next.insert(path, (now_stamp, team));
                            }
                            Err(p) => self.diagnostics.push(diag(
                                &self.roots.home,
                                DiagSource::Team,
                                p,
                                &path,
                            )),
                        }
                    }
                }
            }
        }
        self.teams = next;
    }

    fn refresh_repos(&mut self, now: At) {
        let cwds: HashSet<PathBuf> = self
            .live
            .iter()
            .map(|l| l.reg.cwd.clone())
            .chain(
                self.exited_jobs(now)
                    .filter_map(|(_, j)| j.worktree_path.clone()),
            )
            .collect();
        self.repos.retain(|cwd, _| cwds.contains(cwd));
        for cwd in cwds {
            self.repos
                .entry(cwd)
                .or_insert_with_key(|cwd| sidecars::repo_root(cwd));
        }
    }

    pub fn now(&self) -> At {
        self.host.now()
    }

    pub fn sessions(&self) -> impl Iterator<Item = SessionView<'_>> {
        self.live.iter().map(|l| self.view(l))
    }

    fn view<'a>(&'a self, live: &'a Live) -> SessionView<'a> {
        let Live {
            dir,
            reg,
            transcript,
        } = live;
        let tracked = transcript.as_ref().and_then(|p| self.transcripts.get(p));
        SessionView {
            dir,
            reg,
            job: self.job(dir, reg),
            facts: tracked.map(|t| t.transcript.facts()),
            subagents: tracked.map(|t| &t.subagents),
            tasks: self
                .tasks
                .get(&(dir.clone(), reg.session_id.clone()))
                .map_or(&[], |(_, tasks)| tasks.as_slice()),
            statusline: self.statusline.get(&reg.session_id),
            team: self
                .teams
                .iter()
                .find(|(path, (_, team))| {
                    path.starts_with(&dir.0) && team.includes(&reg.session_id, reg.pane.as_ref())
                })
                .map(|(_, (_, team))| team),
            repo: self.repos.get(&reg.cwd).and_then(|r| r.as_deref()),
            panes: &self.panes,
        }
    }

    pub fn exited(&self, now: At) -> Vec<ExitedView<'_>> {
        self.exited_jobs(now)
            .map(|(dir, job)| ExitedView {
                dir,
                job,
                facts: job
                    .transcript
                    .as_ref()
                    .and_then(|p| self.transcripts.get(p))
                    .map(|t| t.transcript.facts()),
            })
            .collect()
    }

    pub fn watch_roots(&self) -> Vec<PathBuf> {
        self.dirs
            .iter()
            .flat_map(|d| {
                [
                    registry::sessions_dir(d),
                    jobs::jobs_dir(d),
                    sidecars::tasks_root(d),
                ]
            })
            .chain([sidecars::statusline_dir(&self.roots.cache)])
            .filter(|p| p.is_dir())
            .collect()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

fn diag(home: &Path, source: DiagSource, problem: Problem, path: &Path) -> Diagnostic {
    let path = match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    };
    Diagnostic {
        source,
        problem,
        path,
    }
}

fn glob_transcript(dir: &ConfigDir, sid: &SessionId) -> Option<PathBuf> {
    let name = format!("{}.jsonl", sid.0);
    std::fs::read_dir(dir.0.join("projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(&name))
        .filter_map(|p| FileStamp::of(&p).ok().map(|s| (s.mtime_ms, p)))
        .max()
        .map(|(_, p)| p)
}

fn task_files(dir: &Path) -> Vec<(PathBuf, FileStamp)> {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(u32, PathBuf, FileStamp)> = listing
        .flatten()
        .filter_map(|e| {
            let n: u32 = e
                .file_name()
                .to_str()?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            let path = e.path();
            let s = FileStamp::of(&path).ok()?;
            Some((n, path, s))
        })
        .collect();
    files.sort_by_key(|(n, _, _)| *n);
    files.into_iter().map(|(_, p, s)| (p, s)).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use serde_json::json;

    use super::*;
    use crate::registry::ProcKind;
    use crate::registry::tests::Scratch;

    #[derive(Default)]
    struct HostState {
        now: i64,
        procs: HashMap<Pid, ProcStart>,
        ps_fails: bool,
        panes: Option<Vec<PaneInfo>>,
        ps_calls: Vec<Vec<Pid>>,
    }

    struct FakeHost(Rc<RefCell<HostState>>);

    impl Host for FakeHost {
        fn now(&self) -> At {
            At(self.0.borrow().now)
        }
        fn proc_starts(&mut self, pids: &[Pid]) -> Result<HashMap<Pid, ProcStart>, HostError> {
            let mut s = self.0.borrow_mut();
            s.ps_calls.push(pids.to_vec());
            if s.ps_fails {
                return Err(HostError);
            }
            Ok(pids
                .iter()
                .filter_map(|p| Some((*p, s.procs.get(p)?.clone())))
                .collect())
        }
        fn panes(&mut self) -> Result<Vec<PaneInfo>, HostError> {
            self.0.borrow().panes.clone().ok_or(HostError)
        }
    }

    fn iso(s: &str) -> i64 {
        At::parse_iso(s).unwrap().0
    }

    const NOW: &str = "2026-10-04T12:00:00Z";

    struct Fixture {
        home: Scratch,
        host: Rc<RefCell<HostState>>,
        world: World,
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let home = Scratch::new(name);
            home.mkdir(".claude/sessions");
            let host = Rc::new(RefCell::new(HostState {
                now: iso(NOW),
                panes: Some(Vec::new()),
                ..HostState::default()
            }));
            let roots = Roots {
                home: home.path().to_path_buf(),
                env_config_dir: None,
                cache: home.path().join(".cache/agentty"),
            };
            let world = World::new(
                roots,
                Box::new(FakeHost(host.clone())),
                ReadBudget::default(),
            );
            Fixture { home, host, world }
        }

        fn session(&self, dir: &str, pid: u32, sid: &str, extra: serde_json::Value) {
            let mut record = serde_json::json!({
                "pid": pid, "procStart": format!("start-{pid}"), "sessionId": sid,
                "kind": "interactive", "entrypoint": "cli", "status": "idle",
                "statusUpdatedAt": 1, "cwd": "/nowhere",
            });
            record
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            self.home.write(
                &format!("{dir}/sessions/{pid}.json"),
                record.to_string().as_bytes(),
            );
        }

        fn alive(&self, pid: u32) {
            self.host
                .borrow_mut()
                .procs
                .insert(Pid(pid), ProcStart::new(&format!("start-{pid}  ")));
        }

        fn job(&self, dir: &str, id: &str, json: &str) {
            self.home
                .write(&format!("{dir}/jobs/{id}/state.json"), json.as_bytes());
        }

        fn pids(&self) -> Vec<(String, u32)> {
            self.world
                .sessions()
                .map(|v| (v.dir.short().to_owned(), v.reg.pid.0))
                .collect()
        }

        fn exited_ids(&self) -> Vec<String> {
            let now = self.world.now();
            self.world
                .exited(now)
                .iter()
                .map(|e| e.job.id.0.clone())
                .collect()
        }
    }

    #[test]
    fn spare_and_parked_records_are_hidden_but_keep_their_jobs_live() {
        let f = Fixture::new("world-hidden");
        f.session(".claude", 10, "s-10", json!({}));
        f.session(
            ".claude",
            11,
            "s-11",
            json!({"kind": "bg", "jobId": "aaaa0001", "spare": true}),
        );
        f.session(".claude", 12, "s-12", json!({"parkedJobId": "bbbb0002"}));
        f.job(
            ".claude",
            "bbbb0002",
            r#"{"state":"done","lastTerminalAt":"2026-10-04T11:00:00Z"}"#,
        );
        for pid in [10, 11, 12] {
            f.alive(pid);
        }
        let mut f = f;
        f.world.refresh(Wake::Tick);
        assert_eq!(f.pids(), [("".to_owned(), 10)]);
        assert!(f.exited_ids().is_empty());
        assert!(
            f.world.diagnostics().is_empty(),
            "{:?}",
            f.world.diagnostics()
        );
    }

    #[test]
    fn stale_record_and_reused_pid_are_dropped() {
        let mut f = Fixture::new("world-dead");
        f.session(".claude", 20, "s-20", json!({}));
        f.session(".claude", 21, "s-21", json!({}));
        f.session(".claude", 22, "s-22", json!({}));
        f.alive(22);
        f.host
            .borrow_mut()
            .procs
            .insert(Pid(21), ProcStart::new("Mon Oct  5 09:00:00 2026"));
        f.world.refresh(Wake::Tick);
        assert_eq!(f.pids(), [("".to_owned(), 22)]);
        assert!(f.world.diagnostics().is_empty());
    }

    #[test]
    fn session_id_change_keeps_the_row_and_re_resolves_the_transcript() {
        let mut f = Fixture::new("world-clear");
        f.session(".claude", 30, "old-sid", json!({}));
        f.alive(30);
        f.home
            .write(".claude/projects/-work-demo/old-sid.jsonl", b"");
        f.world.refresh(Wake::Doorbell);
        assert_eq!(
            f.world.live[0].transcript,
            Some(
                f.home
                    .path()
                    .join(".claude/projects/-work-demo/old-sid.jsonl")
            )
        );

        f.session(".claude", 30, "new-sid", json!({}));
        f.home
            .write(".claude/projects/-work-other/new-sid.jsonl", b"");
        f.world.refresh(Wake::Doorbell);
        let rows: Vec<_> = f
            .world
            .sessions()
            .map(|v| (v.reg.pid, v.reg.session_id.0.clone()))
            .collect();
        assert_eq!(rows, [(Pid(30), "new-sid".to_owned())]);
        assert_eq!(
            f.world.live[0].transcript,
            Some(
                f.home
                    .path()
                    .join(".claude/projects/-work-other/new-sid.jsonl")
            )
        );
        assert_eq!(f.host.borrow().ps_calls, [vec![Pid(30)]]);
    }

    #[test]
    fn missing_transcript_is_reglobbed_after_the_backoff() {
        let mut f = Fixture::new("world-reglob");
        f.session(".claude", 31, "late", json!({}));
        f.alive(31);
        f.world.refresh(Wake::Doorbell);
        assert_eq!(f.world.live[0].transcript, None);
        f.home.write(".claude/projects/-p/late.jsonl", b"");
        f.host.borrow_mut().now += REGLOB_MS - 1;
        f.world.refresh(Wake::Doorbell);
        assert_eq!(f.world.live[0].transcript, None);
        f.host.borrow_mut().now += 1;
        f.world.refresh(Wake::Doorbell);
        assert_eq!(
            f.world.live[0].transcript,
            Some(f.home.path().join(".claude/projects/-p/late.jsonl"))
        );
    }

    #[test]
    fn bg_transcript_comes_from_link_scan_path() {
        let mut f = Fixture::new("world-link");
        f.session(
            ".claude",
            32,
            "bg-sid",
            json!({"kind": "bg", "jobId": "cafe0001"}),
        );
        f.alive(32);
        f.job(
            ".claude",
            "cafe0001",
            r#"{"state":"working","linkScanPath":"/exact/path.jsonl"}"#,
        );
        f.home.write(".claude/projects/-p/bg-sid.jsonl", b"");
        f.world.refresh(Wake::Doorbell);
        let view = f.world.sessions().next().unwrap();
        assert_eq!(view.reg.kind, ProcKind::Bg);
        assert_eq!(view.job.map(|j| j.id.0.as_str()), Some("cafe0001"));
        assert_eq!(
            f.world.live[0].transcript,
            Some(PathBuf::from("/exact/path.jsonl"))
        );
    }

    #[test]
    fn two_config_dirs_each_contribute_rows_and_watch_roots() {
        let mut f = Fixture::new("world-dirs");
        f.session(".claude", 40, "a", json!({}));
        f.session(".claude-personal", 41, "b", json!({}));
        f.home.mkdir(".claude-personal/jobs");
        f.home.mkdir(".claude-stale");
        f.alive(40);
        f.alive(41);
        f.world.refresh(Wake::Tick);
        assert_eq!(f.pids(), [("".to_owned(), 40), ("personal".to_owned(), 41)]);
        let h = f.home.path();
        assert_eq!(
            f.world.watch_roots(),
            [
                h.join(".claude/sessions"),
                h.join(".claude-personal/sessions"),
                h.join(".claude-personal/jobs")
            ]
        );
    }

    #[test]
    fn exited_jobs_are_shown_for_24h_after_their_process_retires() {
        let mut f = Fixture::new("world-exited");
        f.job(
            ".claude",
            "c0000001",
            r#"{"state":"done","lastTerminalAt":"2026-10-04T11:00:00Z"}"#,
        );
        f.job(
            ".claude",
            "c0000002",
            r#"{"state":"done","lastTerminalAt":"2026-10-03T11:59:00Z"}"#,
        );
        f.job(
            ".claude",
            "c0000003",
            r#"{"state":"done","lastTerminalAt":"2026-10-04T11:30:00Z"}"#,
        );
        f.session(
            ".claude",
            50,
            "s-50",
            json!({"kind": "bg", "jobId": "c0000003"}),
        );
        f.alive(50);
        f.job(
            ".claude",
            "c0000004",
            r#"{"state":"working","updatedAt":"2026-10-04T10:00:00Z"}"#,
        );
        f.session(
            ".claude",
            51,
            "s-51",
            json!({"kind": "bg", "jobId": "c0000004"}),
        );
        f.home.write(".claude/jobs/pins.json", b"{}");
        f.home.mkdir(".claude/jobs/spare001");
        f.world.refresh(Wake::Tick);
        assert_eq!(f.exited_ids(), ["c0000001", "c0000004"]);
        assert_eq!(f.pids(), [("".to_owned(), 50)]);
    }

    #[test]
    fn ps_failure_keeps_every_candidate_and_reports_it() {
        let mut f = Fixture::new("world-ps");
        f.session(".claude", 60, "s-60", json!({}));
        f.session(".claude", 61, "s-61", json!({}));
        f.alive(60);
        f.world.refresh(Wake::Tick);
        assert_eq!(f.pids(), [("".to_owned(), 60)]);

        f.host.borrow_mut().ps_fails = true;
        f.world.refresh(Wake::Tick);
        assert_eq!(f.pids(), [("".to_owned(), 60), ("".to_owned(), 61)]);
        let ps = Diagnostic {
            source: DiagSource::Ps,
            problem: Problem::Unavailable,
            path: "ps".into(),
        };
        assert_eq!(f.world.diagnostics(), [ps.clone()]);

        f.home.remove(".claude/sessions/61.json");
        f.world.refresh(Wake::Doorbell);
        assert_eq!(f.pids(), [("".to_owned(), 60)]);
        assert_eq!(f.world.diagnostics(), [ps]);

        f.host.borrow_mut().ps_fails = false;
        f.world.refresh(Wake::Tick);
        assert!(f.world.diagnostics().is_empty());
    }

    #[test]
    fn tmux_snapshot_reaches_views_and_its_failure_is_reported() {
        let mut f = Fixture::new("world-tmux");
        f.session(".claude", 70, "s-70", json!({"tmux": "0:@1.%9"}));
        f.alive(70);
        let pane = PaneInfo {
            id: PaneId::parse("%9").unwrap(),
            pid: Pid(69),
        };
        f.host.borrow_mut().panes = Some(vec![pane.clone()]);
        f.world.refresh(Wake::Tick);
        assert_eq!(f.world.sessions().next().unwrap().panes, [pane]);

        f.host.borrow_mut().panes = None;
        f.world.refresh(Wake::Tick);
        assert!(f.world.sessions().next().unwrap().panes.is_empty());
        assert_eq!(f.world.diagnostics()[0].source, DiagSource::Tmux);
    }

    #[test]
    fn sidecars_follow_their_files_and_leave_with_the_session() {
        let mut f = Fixture::new("world-sidecars");
        f.session(".claude", 80, "s-80", json!({}));
        f.alive(80);
        f.home.write(
            ".claude/tasks/s-80/2.json",
            br#"{"id":"2","subject":"Second","status":"pending"}"#,
        );
        f.home.write(
            ".claude/tasks/s-80/1.json",
            br#"{"id":"1","subject":"First","status":"in_progress"}"#,
        );
        f.home.write(".claude/tasks/s-80/.lock", b"");
        f.home.write(".claude/tasks/s-80/1.json.lock", b"");
        f.home.write(".cache/agentty/statusline/s-80.json", br#"{"context_window":{"used_percentage":42.5,"context_window_size":1000000},"cost":{"total_cost_usd":1.25}}"#);
        f.world.refresh(Wake::Doorbell);
        {
            let v = f.world.sessions().next().unwrap();
            let subjects: Vec<_> = v.tasks.iter().map(|t| t.subject.as_str()).collect();
            assert_eq!(subjects, ["First", "Second"]);
            let sl = v.statusline.unwrap();
            assert_eq!(
                (sl.ctx_used_pct, sl.window, sl.cost_usd),
                (Some(42.5), Some(1_000_000), Some(1.25))
            );
        }

        f.home.write(
            ".claude/tasks/s-80/2.json",
            br#"{"id":"2","subject":"Second","status":"completed"}"#,
        );
        f.world.refresh(Wake::Doorbell);
        let status = f.world.sessions().next().unwrap().tasks[1].status.clone();
        assert_eq!(status, sidecars::TaskStatus::Completed);

        f.home.remove(".claude/tasks/s-80/1.json");
        f.home.remove(".claude/tasks/s-80/2.json");
        f.world.refresh(Wake::Doorbell);
        assert!(f.world.sessions().next().unwrap().tasks.is_empty());

        f.home.remove(".claude/sessions/80.json");
        f.world.refresh(Wake::Doorbell);
        assert_eq!(f.world.sessions().count(), 0);
        assert!(
            f.world.tasks.is_empty()
                && f.world.statusline.is_empty()
                && f.world.resolved.is_empty()
        );
    }

    #[test]
    fn team_lead_and_repo_root_reach_the_view() {
        let mut f = Fixture::new("world-team");
        let repo = f.home.path().join("src/demo");
        f.home.mkdir("src/demo/.git");
        f.home.mkdir("src/demo/sub");
        let cwd = repo.join("sub");
        f.session(".claude", 90, "lead-sid", json!({"cwd": cwd}));
        f.session(".claude", 91, "member-sid", json!({"tmux": "0:@3.%12"}));
        f.session(".claude", 92, "outsider-sid", json!({"tmux": "0:@3.%13"}));
        for pid in [90, 91, 92] {
            f.alive(pid);
        }
        f.home.write(
            ".claude/teams/session-lead/config.json",
            br#"{"name":"session-lead","leadSessionId":"lead-sid","members":[{"name":"w1","tmuxPaneId":"%12","isActive":true}]}"#,
        );
        f.world.refresh(Wake::Doorbell);
        let teams: Vec<(u32, Option<&str>)> = f
            .world
            .sessions()
            .map(|v| (v.reg.pid.0, v.team.map(|t| t.name.as_str())))
            .collect();
        assert_eq!(
            teams,
            [
                (90, Some("session-lead")),
                (91, Some("session-lead")),
                (92, None)
            ],
            "the lead and the pane member get the team, the outsider does not"
        );
        let v = f.world.sessions().next().unwrap();
        assert_eq!(
            v.repo,
            Some(std::fs::canonicalize(&repo).unwrap().as_path())
        );
    }

    #[test]
    fn unparsable_job_keeps_last_good_parse_and_is_reported() {
        let mut f = Fixture::new("world-badjob");
        f.session(
            ".claude",
            95,
            "s-95",
            json!({"kind": "bg", "jobId": "dead0001"}),
        );
        f.alive(95);
        f.job(
            ".claude",
            "dead0001",
            r#"{"state":"working","name":"keep me"}"#,
        );
        f.world.refresh(Wake::Doorbell);
        f.job(".claude", "dead0001", r#"{"state":"work"#);
        f.world.refresh(Wake::Doorbell);
        let name = f
            .world
            .sessions()
            .next()
            .unwrap()
            .job
            .and_then(|j| j.name.clone());
        assert_eq!(name.as_deref(), Some("keep me"));
        let d = &f.world.diagnostics()[0];
        assert_eq!(
            (d.source, &d.problem, d.path.as_str()),
            (
                DiagSource::Job,
                &Problem::Unparsable,
                "~/.claude/jobs/dead0001/state.json"
            )
        );
    }

    #[test]
    fn read_budget_defers_first_sight_windows_to_later_ticks() {
        let mut f = Fixture::new("world-budget");
        let filler = "{\"type\":\"x\"}\n".repeat(5 << 20 >> 4);
        for (pid, sid) in [(100, "big-a"), (101, "big-b")] {
            f.session(".claude", pid, sid, json!({"status": "busy"}));
            f.alive(pid);
            f.home.write(
                &format!(".claude/projects/-p/{sid}.jsonl"),
                filler.as_bytes(),
            );
        }
        f.world.refresh(Wake::Tick);
        let opened = |f: &Fixture| f.world.sessions().filter(|v| v.facts.is_some()).count();
        assert_eq!(opened(&f), 1);
        let deferred: Vec<_> = f
            .world
            .diagnostics()
            .iter()
            .filter(|d| d.problem == Problem::Deferred)
            .collect();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].path, "~/.claude/projects/-p/big-b.jsonl");

        f.world.refresh(Wake::Tick);
        assert_eq!(opened(&f), 2);
        assert!(
            f.world.diagnostics().is_empty(),
            "{:?}",
            f.world.diagnostics()
        );

        f.home.remove(".claude/sessions/100.json");
        f.world.refresh(Wake::Doorbell);
        assert_eq!(f.world.transcripts.len(), 2);
        f.world.refresh(Wake::Tick);
        assert_eq!(
            f.world.transcripts.keys().collect::<Vec<_>>(),
            [&f.home.path().join(".claude/projects/-p/big-b.jsonl")]
        );
    }

    #[test]
    #[ignore = "reads the real ~/.claude*; run with --ignored --nocapture"]
    fn real_data_probe() {
        let mut world = World::new(
            Roots::from_env().unwrap(),
            Box::new(RealHost),
            ReadBudget::default(),
        );
        world.refresh(Wake::Doorbell);
        let panes = RealHost.panes().unwrap_or_default();
        for dir in &world.dirs {
            let (entries, problems) = registry::scan(dir);
            let jobs = world.jobs.keys().filter(|(d, _)| d == dir).count();
            println!(
                "dir {:?}: {} records, {} unparsed, {} jobs",
                dir.short(),
                entries.len(),
                problems.len(),
                jobs
            );
        }
        for v in world.sessions() {
            let in_snapshot = v
                .reg
                .pane
                .as_ref()
                .is_some_and(|p| panes.iter().any(|q| &q.id == p));
            println!(
                "live dir={:?} pid={} kind={:?} status={:?} job={} job_parsed={} pane={} pane_in_tmux={} transcript={}",
                v.dir.short(),
                v.reg.pid.0,
                v.reg.kind,
                v.reg.status,
                v.reg.job.is_some(),
                v.job.is_some(),
                v.reg.pane.is_some(),
                in_snapshot,
                world
                    .live
                    .iter()
                    .any(|l| l.reg.pid == v.reg.pid && l.transcript.is_some()),
            );
        }
        let now = world.now();
        for e in world.exited(now) {
            println!(
                "exited dir={:?} state={:?} hours_ago={}",
                e.dir.short(),
                e.job.state,
                now.millis_since(e.job.ended_at.unwrap()) / 3_600_000
            );
        }
        for d in world.diagnostics() {
            println!("diagnostic {:?} {:?}", d.source, d.problem);
        }
    }

    #[test]
    fn list_panes_output_parses_leniently() {
        let panes = parse_panes("%254\t5431\n%250\t85078\nbogus\n%x\t1\n\n");
        assert_eq!(
            panes
                .iter()
                .map(|p| (p.id.as_str(), p.pid.0))
                .collect::<Vec<_>>(),
            [("%254", 5431), ("%250", 85078)]
        );
    }
}
