use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{At, ConfigDir, JobId, PaneId, Pid, Problem, ProcStart, SessionId, WaitingFor};

#[derive(Clone, Debug, PartialEq)]
pub struct RegEntry {
    pub pid: Pid,
    pub proc_start: ProcStart,
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub kind: ProcKind,
    pub entrypoint: String,
    pub status: RegStatus,
    pub status_since: At,
    pub name: Option<Name>,
    pub pane: Option<PaneId>,
    pub remote_control: bool,
    pub job: Option<JobId>,
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcKind {
    Interactive,
    Bg,
    Other(String),
}

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
    updated_at: Option<i64>,
    started_at: Option<i64>,
    name: Option<String>,
    name_source: Option<String>,
    tmux: Option<String>,
    bridge_session_id: Option<String>,
    job_id: Option<String>,
    spare: Option<bool>,
    parked_job_id: Option<String>,
}

pub fn parse(bytes: &[u8]) -> Result<RegEntry, Problem> {
    let w: WireReg = serde_json::from_slice(bytes).map_err(|_| Problem::Unparsable)?;
    let pid = w.pid.ok_or(Problem::MissingField("pid"))?;
    let proc_start = nonempty(w.proc_start).ok_or(Problem::MissingField("procStart"))?;
    let session_id = nonempty(w.session_id).ok_or(Problem::MissingField("sessionId"))?;
    let parked_job = nonempty(w.parked_job_id);
    Ok(RegEntry {
        pid: Pid(pid),
        proc_start: ProcStart::new(&proc_start),
        session_id: SessionId(session_id),
        cwd: w.cwd.unwrap_or_default(),
        kind: match w.kind.as_deref() {
            Some("interactive") => ProcKind::Interactive,
            Some("bg") => ProcKind::Bg,
            other => ProcKind::Other(other.unwrap_or_default().to_owned()),
        },
        entrypoint: w.entrypoint.unwrap_or_default(),
        status: match w.status.as_deref() {
            Some("busy") => RegStatus::Busy,
            Some("shell") => RegStatus::Shell,
            Some("idle") => RegStatus::Idle,
            Some("waiting") => RegStatus::Waiting(waiting_for(w.waiting_for.as_deref())),
            other => RegStatus::Other(other.unwrap_or_default().to_owned()),
        },
        status_since: At(w.status_updated_at.or(w.updated_at).or(w.started_at).unwrap_or(0)),
        name: nonempty(w.name).map(|value| Name {
            value,
            source: match w.name_source.as_deref() {
                Some("user") => NameSource::User,
                Some("auto") => NameSource::Auto,
                Some("derived") => NameSource::Derived,
                _ => NameSource::Other,
            },
        }),
        pane: w.tmux.as_deref().and_then(PaneId::parse),
        remote_control: nonempty(w.bridge_session_id).is_some(),
        hidden: w.spare == Some(true) || parked_job.is_some(),
        job: nonempty(w.job_id).or(parked_job).as_deref().and_then(JobId::parse),
    })
}

fn waiting_for(raw: Option<&str>) -> WaitingFor {
    match raw {
        None => WaitingFor::Unspecified,
        Some("permission prompt") => WaitingFor::Permission,
        Some("input needed") => WaitingFor::Input,
        Some("dialog open") => WaitingFor::Dialog,
        Some("sandbox request") => WaitingFor::Sandbox,
        Some("worker request") => WaitingFor::Worker,
        Some("goal proposal") => WaitingFor::GoalProposal,
        Some(other) => WaitingFor::Other(other.to_owned()),
    }
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

pub fn scan(dir: &ConfigDir) -> (Vec<RegEntry>, Vec<(PathBuf, Problem)>) {
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    let Ok(listing) = std::fs::read_dir(sessions_dir(dir)) else {
        return (entries, problems);
    };
    for path in listing.flatten().map(|e| e.path()) {
        if !is_registry_name(&path) {
            continue;
        }
        match parse_with_retry(|| std::fs::read(&path)) {
            Ok(entry) => entries.push(entry),
            Err(None) => {}
            Err(Some(problem)) => problems.push((path, problem)),
        }
    }
    (entries, problems)
}

fn is_registry_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".json"))
        .is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
}

// Claude Code rewrites the registry file in place without a rename (2.1.289), so a read can
// land on a half-written file.
fn parse_with_retry(
    mut read: impl FnMut() -> std::io::Result<Vec<u8>>,
) -> Result<RegEntry, Option<Problem>> {
    let mut attempt = || match read() {
        Ok(bytes) => parse(&bytes).map_err(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(None),
        Err(_) => Err(Some(Problem::Unreadable)),
    };
    match attempt() {
        Err(Some(_)) => {
            std::thread::sleep(std::time::Duration::from_millis(25));
            attempt()
        }
        first => first,
    }
}

pub fn sessions_dir(dir: &ConfigDir) -> PathBuf {
    dir.0.join("sessions")
}

pub fn ps_argv(pids: &[Pid]) -> Vec<String> {
    let list = pids.iter().map(|p| p.0.to_string()).collect::<Vec<_>>().join(",");
    vec!["-o".into(), "pid=,lstart=".into(), "-p".into(), list]
}

pub fn parse_ps(stdout: &str) -> HashMap<Pid, ProcStart> {
    stdout
        .lines()
        .filter_map(|line| {
            let (pid, start) = line.trim_start().split_once(char::is_whitespace)?;
            let start = ProcStart::new(start);
            (!start.as_str().is_empty()).then_some((Pid(pid.parse().ok()?), start))
        })
        .collect()
}

pub fn discover_config_dirs(home: &Path, env_override: Option<&Path>) -> Vec<ConfigDir> {
    let mut found = vec![home.join(".claude")];
    if let Ok(listing) = std::fs::read_dir(home) {
        let mut extra: Vec<PathBuf> = listing
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(".claude-")))
            .map(|e| e.path())
            .filter(|p| p.join("sessions").is_dir())
            .collect();
        extra.sort();
        found.extend(extra);
    }
    found.extend(env_override.map(Path::to_path_buf));

    let mut seen = std::collections::HashSet::new();
    found
        .into_iter()
        .filter(|p| p.is_dir())
        .filter(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
        .map(ConfigDir)
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    pub(crate) struct Scratch(PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Scratch {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("agentically-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }
        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
        pub(crate) fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        pub(crate) fn mkdir(&self, rel: &str) {
            std::fs::create_dir_all(self.0.join(rel)).unwrap();
        }
        pub(crate) fn remove(&self, rel: &str) {
            let path = self.0.join(rel);
            if path.is_dir() {
                std::fs::remove_dir_all(path).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }


    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/registry/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn waiting_record_keeps_reason_pane_and_status_time() {
        let e = parse(&fixture("interactive_waiting.json")).unwrap();
        assert_eq!(e.status, RegStatus::Waiting(WaitingFor::Permission));
        assert_eq!(e.pane.as_ref().map(PaneId::as_str), Some("%251"));
        assert_eq!(e.status_since, At(1791150090000));
        assert_eq!(e.proc_start.as_str(), "Sun Oct  4 21:17:02 2026");
        assert_eq!(e.kind, ProcKind::Interactive);
        assert_eq!(e.name, Some(Name { value: "demo-repo-e7".into(), source: NameSource::Derived }));
        assert!(!e.hidden && !e.remote_control && e.job.is_none());
    }

    #[test]
    fn waiting_without_reason_is_unspecified_not_idle() {
        let e = parse(br#"{"pid":1,"procStart":"x","sessionId":"s","status":"waiting"}"#).unwrap();
        assert_eq!(e.status, RegStatus::Waiting(WaitingFor::Unspecified));
    }

    #[test]
    fn spare_is_hidden_and_keeps_its_job() {
        let e = parse(&fixture("bg_spare.json")).unwrap();
        assert!(e.hidden);
        assert_eq!(e.kind, ProcKind::Bg);
        assert_eq!(e.job, Some(JobId("abcdef01".into())));
        assert_eq!(e.name, None);
        assert_eq!(e.pane, None);
    }

    #[test]
    fn parked_interactive_is_hidden_and_points_at_its_job() {
        let e = parse(br#"{"pid":5,"procStart":"x","sessionId":"s","kind":"interactive","status":"idle","parkedJobId":"0badf00d"}"#).unwrap();
        assert!(e.hidden);
        assert_eq!(e.job, Some(JobId("0badf00d".into())));
    }

    #[test]
    fn unknown_enum_values_survive_as_other() {
        let e = parse(&fixture("future_values.json")).unwrap();
        assert_eq!(e.status, RegStatus::Other("napping".into()));
        assert_eq!(e.kind, ProcKind::Other("daemon-worker".into()));
        assert_eq!(e.name.unwrap().source, NameSource::Other);
        assert!(e.remote_control);
        assert_eq!(e.status_since, At(1791150200000));
        let w = parse(br#"{"pid":1,"procStart":"x","sessionId":"s","status":"waiting","waitingFor":"oracle consult"}"#).unwrap();
        assert_eq!(w.status, RegStatus::Waiting(WaitingFor::Other("oracle consult".into())));
    }

    #[test]
    fn identity_fields_are_required_and_named() {
        assert_eq!(parse(br#"{"procStart":"x","sessionId":"s"}"#), Err(Problem::MissingField("pid")));
        assert_eq!(parse(br#"{"pid":1,"sessionId":"s"}"#), Err(Problem::MissingField("procStart")));
        assert_eq!(parse(br#"{"pid":1,"procStart":"x"}"#), Err(Problem::MissingField("sessionId")));
        assert_eq!(parse(br#"{"pid":1,"procSt"#), Err(Problem::Unparsable));
    }

    #[test]
    fn torn_read_is_retried_once() {
        let good = fixture("interactive_waiting.json");
        let mut reads = vec![Ok(good[..40].to_vec()), Ok(good.clone())].into_iter();
        let e = parse_with_retry(|| reads.next().unwrap()).unwrap();
        assert_eq!(e.pid, Pid(4242));

        let mut calls = 0;
        let still_torn = parse_with_retry(|| {
            calls += 1;
            Ok(b"{\"pid\":".to_vec())
        });
        assert_eq!(still_torn, Err(Some(Problem::Unparsable)));
        assert_eq!(calls, 2);
    }

    #[test]
    fn ps_output_is_trimmed_and_dead_pids_are_absent() {
        let out = "    1 Mon Sep 28 08:09:25 2026    \n4242 Sun Oct  4 21:17:02 2026    \ngarbage\n";
        let starts = parse_ps(out);
        assert_eq!(starts.len(), 2);
        let reg = parse(&fixture("interactive_waiting.json")).unwrap();
        assert_eq!(starts.get(&Pid(4242)), Some(&reg.proc_start));
        assert_eq!(starts.get(&Pid(1)).map(ProcStart::as_str), Some("Mon Sep 28 08:09:25 2026"));
        assert_eq!(ps_argv(&[Pid(1), Pid(4242)]), ["-o", "pid=,lstart=", "-p", "1,4242"]);
    }

    #[test]
    fn scan_reads_only_pid_records() {
        let home = Scratch::new("registry-scan");
        let dir = ConfigDir(home.path().join(".claude"));
        home.write(".claude/sessions/4242.json", &fixture("interactive_waiting.json"));
        home.write(".claude/sessions/4242.abc123.key", b"SECRET_SENTINEL_x9");
        home.write(".claude/sessions/.fleetview-heartbeat", b"");
        home.write(".claude/sessions/77.json", b"{\"pid\":77}");
        let (entries, problems) = scan(&dir);
        assert_eq!(entries.iter().map(|e| e.pid).collect::<Vec<_>>(), [Pid(4242)]);
        assert_eq!(problems, [(sessions_dir(&dir).join("77.json"), Problem::MissingField("procStart"))]);
    }

    #[test]
    fn discovery_finds_default_suffixed_and_env_dirs_once() {
        let home = Scratch::new("registry-discover");
        home.mkdir(".claude/sessions");
        home.mkdir(".claude-personal/sessions");
        home.mkdir(".claude-backup");
        home.mkdir("elsewhere/cfg");
        home.write(".claude.json", b"{}");
        let h = home.path();
        let dirs = discover_config_dirs(h, Some(&h.join(".claude-personal")));
        assert_eq!(dirs, [ConfigDir(h.join(".claude")), ConfigDir(h.join(".claude-personal"))]);
        let dirs = discover_config_dirs(h, Some(&h.join("elsewhere/cfg")));
        assert_eq!(dirs.len(), 3);
        assert_eq!(dirs.last(), Some(&ConfigDir(h.join("elsewhere/cfg"))));
    }
}
