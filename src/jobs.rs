use std::path::PathBuf;

use serde::Deserialize;

use crate::model::{At, ConfigDir, JobId, Problem, SafeText, SessionId, nonempty};

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: JobId,
    pub session_id: Option<SessionId>,
    pub state: JobState,
    pub tempo: Option<Tempo>,
    pub detail: Option<SafeText>,
    pub needs: Option<SafeText>,
    pub fan: Vec<FanItem>,
    pub transcript: Option<PathBuf>,
    pub worktree_path: Option<PathBuf>,
    pub worktree_branch: Option<SafeText>,
    pub name: Option<String>,
    pub remote_control: bool,
    pub prs: Vec<u32>,
    pub updated_at: Option<At>,
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

#[derive(Clone, Debug, PartialEq)]
pub struct FanItem {
    pub id: String,
    pub kind: FanKind,
    pub label: SafeText,
    pub started_at: Option<At>,
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
    fan: Option<Vec<WireFan>>,
    link_scan_path: Option<PathBuf>,
    worktree_path: Option<PathBuf>,
    worktree_branch: Option<String>,
    name: Option<String>,
    bridge_session_id: Option<String>,
    children: Option<Vec<WireChild>>,
    updated_at: Option<String>,
    last_terminal_at: Option<String>,
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
    id: Option<serde_json::Value>,
    kind: Option<String>,
    href: Option<String>,
}

pub fn parse(id: JobId, bytes: &[u8]) -> Result<Job, Problem> {
    let w: WireJob = serde_json::from_slice(bytes).map_err(|_| Problem::Unparsable)?;
    let updated_at = w.updated_at.as_deref().and_then(At::parse_iso);
    let mut prs: Vec<u32> = Vec::new();
    for n in w
        .children
        .iter()
        .flatten()
        .filter(|c| c.kind.as_deref() == Some("pr"))
        .filter_map(pr_number)
    {
        if !prs.contains(&n) {
            prs.push(n);
        }
    }
    Ok(Job {
        id,
        session_id: nonempty(w.session_id).map(SessionId),
        state: match w.state.as_deref().unwrap_or_default() {
            "starting" => JobState::Starting,
            "running" => JobState::Running,
            "working" => JobState::Working,
            "blocked" => JobState::Blocked,
            "done" => JobState::Done,
            "failed" => JobState::Failed,
            "stopped" => JobState::Stopped,
            other => JobState::Other(other.to_owned()),
        },
        tempo: w.tempo.map(|t| match t.as_str() {
            "active" => Tempo::Active,
            "idle" => Tempo::Idle,
            "blocked" => Tempo::Blocked,
            _ => Tempo::Other(t),
        }),
        detail: nonempty(w.detail).map(|d| SafeText::redact(&d)),
        needs: w.needs.as_ref().and_then(question).map(SafeText::redact),
        fan: w.fan.into_iter().flatten().map(fan_item).collect(),
        transcript: w.link_scan_path,
        worktree_path: w.worktree_path,
        worktree_branch: nonempty(w.worktree_branch).map(|b| SafeText::redact(&b)),
        name: nonempty(w.name),
        remote_control: nonempty(w.bridge_session_id).is_some(),
        prs,
        updated_at,
        ended_at: w
            .last_terminal_at
            .as_deref()
            .and_then(At::parse_iso)
            .or(updated_at),
    })
}

fn question(needs: &serde_json::Value) -> Option<&str> {
    let q = match needs {
        serde_json::Value::String(s) => s.as_str(),
        serde_json::Value::Object(o) => o.get("question")?.as_str()?,
        _ => return None,
    };
    (!q.trim().is_empty()).then_some(q)
}

fn pr_number(child: &WireChild) -> Option<u32> {
    let from_id = || match child.id.as_ref()? {
        serde_json::Value::String(s) => s.parse().ok(),
        n => n.as_u64()?.try_into().ok(),
    };
    let from_href = || {
        child
            .href
            .as_deref()?
            .rsplit_once("/pull/")?
            .1
            .split(['/', '#', '?'])
            .next()?
            .parse()
            .ok()
    };
    from_id().or_else(from_href)
}

fn fan_item(w: WireFan) -> FanItem {
    let kind = match w.kind.as_deref().unwrap_or_default() {
        "agent" => FanKind::Agent,
        "workflow" => FanKind::Workflow,
        "shell" => FanKind::Shell,
        "monitor" => FanKind::Monitor,
        "mcp" => FanKind::Mcp,
        "todo" => FanKind::Todo,
        other => FanKind::Other(other.to_owned()),
    };
    let raw = w.label.unwrap_or_default();
    let label = match kind {
        FanKind::Shell => SafeText::redact(
            SafeText::redact(&raw)
                .as_str()
                .split(' ')
                .next()
                .unwrap_or(""),
        ),
        _ => SafeText::redact(&raw),
    };
    FanItem {
        id: w.id.unwrap_or_default(),
        kind,
        label,
        started_at: w.started_at.map(At),
        done_at: w.done_at.map(At),
        failed: w.failed.unwrap_or(false),
    }
}

pub fn jobs_dir(dir: &ConfigDir) -> PathBuf {
    dir.0.join("jobs")
}

impl Job {
    pub fn open_work(&self) -> impl Iterator<Item = &FanItem> {
        self.fan
            .iter()
            .filter(|f| f.done_at.is_none() && f.kind != FanKind::Todo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/jobs/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn live() -> Job {
        parse(JobId("c0ffee00".into()), &fixture("live_state.json")).unwrap()
    }

    #[test]
    fn secrets_in_provider_env_and_shell_labels_never_materialise() {
        let job = live();
        assert!(!format!("{job:?}").contains("SECRET_SENTINEL"), "{job:?}");
        let shell = job.fan.iter().find(|f| f.kind == FanKind::Shell).unwrap();
        assert_eq!(shell.label.as_str(), "npm");
    }

    #[test]
    fn live_state_maps_every_used_field() {
        let job = live();
        assert_eq!(job.id, JobId("c0ffee00".into()));
        assert_eq!(
            job.session_id,
            Some(SessionId("c0ffee00-1111-4222-8333-444444444444".into()))
        );
        assert_eq!(
            (job.state.clone(), job.tempo.clone()),
            (JobState::Working, Some(Tempo::Active))
        );
        assert_eq!(
            job.detail.as_ref().map(SafeText::as_str),
            Some("Fee logic done; client side next")
        );
        assert_eq!(job.needs, None);
        assert_eq!(job.prs, [1346, 1347]);
        assert!(!job.remote_control);
        assert_eq!(job.updated_at, At::parse_iso("2026-10-04T10:15:30.500Z"));
        assert_eq!(
            job.ended_at, job.updated_at,
            "no lastTerminalAt: fall back to updatedAt"
        );
        assert_eq!(job.worktree_branch, Some(SafeText::ours("fees")));
        assert!(
            job.transcript
                .as_ref()
                .is_some_and(|p| p.ends_with("c0ffee00-1111-4222-8333-444444444444.jsonl"))
        );
    }

    #[test]
    fn open_work_excludes_done_items_and_todos() {
        let job = live();
        let open: Vec<_> = job.open_work().map(|f| f.id.as_str()).collect();
        assert_eq!(open, ["a1", "b1", "x1"]);
        let monitor = job.fan.iter().find(|f| f.id == "b2").unwrap();
        assert_eq!(
            (monitor.done_at, monitor.failed),
            (Some(At(1791150009000)), true)
        );
        assert_eq!(
            job.fan
                .iter()
                .find(|f| f.id == "todo:2")
                .unwrap()
                .started_at,
            None
        );
    }

    #[test]
    fn unknown_values_and_null_collections_degrade() {
        let job = live();
        assert_eq!(job.fan.last().unwrap().kind, FanKind::Other("cron".into()));
        let job = parse(JobId("b10cked0".into()), &fixture("blocked_future.json")).unwrap();
        assert_eq!(job.state, JobState::Other("resuming".into()));
        assert_eq!(job.tempo, Some(Tempo::Other("dozing".into())));
        assert_eq!(
            job.needs.as_ref().map(SafeText::as_str),
            Some("Which database should the migration target?")
        );
        assert!(job.fan.is_empty() && job.prs.is_empty());
        assert!(job.remote_control);
        assert_eq!(job.ended_at, At::parse_iso("2026-10-04T10:05:00Z"));
    }

    #[test]
    fn parse_failure_is_a_category_without_content() {
        let torn = br#"{"state":"working","providerEnv":{"K":"SECRET_SENTINEL_x9""#;
        assert_eq!(parse(JobId("x".into()), torn), Err(Problem::Unparsable));
    }
}
