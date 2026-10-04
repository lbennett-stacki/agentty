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
    pub tool_use: Option<ToolUseId>,
    pub parent: Option<AgentId>,
    pub model: Option<String>,
    pub stopped_by_user: bool,
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

struct Entry {
    meta_path: PathBuf,
    meta_mtime_ms: i64,
    meta: AgentMeta,
    kept: Option<Facts>,
}

pub struct Subagents {
    dir: PathBuf,
    dir_mtime_ms: Option<i64>,
    entries: BTreeMap<AgentId, Entry>,
    open: BTreeMap<AgentId, Transcript>,
}

impl Subagents {
    pub fn new(main_transcript: &Path) -> Subagents {
        let dir = main_transcript.with_extension("").join("subagents");
        Subagents {
            dir,
            dir_mtime_ms: None,
            entries: BTreeMap::new(),
            open: BTreeMap::new(),
        }
    }

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
                    let path = self.entries[&id]
                        .meta_path
                        .with_extension("")
                        .with_extension("jsonl");
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

    fn drop_cursor(&mut self, id: &AgentId) {
        if let Some(t) = self.open.remove(id) {
            self.entries
                .get_mut(id)
                .expect("every cursor has an entry")
                .kept = Some(t.facts().clone());
        }
    }

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

    fn relist(&mut self, problems: &mut Vec<Problem>) {
        let mut seen: Vec<(AgentId, Option<String>, PathBuf)> = Vec::new();
        for (path, run) in meta_files(&self.dir) {
            let Some(id) = agent_id_of(&path) else {
                continue;
            };
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
                                    Entry {
                                        meta_path: path,
                                        meta_mtime_ms: mtime,
                                        meta,
                                        kept: None,
                                    },
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

    // Claude Code merges `stoppedByUser` into an existing meta file in place, which does not
    // move the directory mtime that gates re-listing.
    fn reparse_if_moved(&mut self, id: &AgentId, problems: &mut Vec<Problem>) {
        let Some(e) = self.entries.get_mut(id) else {
            return;
        };
        let Ok(stamp) = FileStamp::of(&e.meta_path) else {
            return;
        };
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

fn agent_id_of(path: &Path) -> Option<AgentId> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_prefix("agent-")?.strip_suffix(".meta.json")?;
    (!id.is_empty()).then(|| AgentId(id.to_owned()))
}

fn meta_files(dir: &Path) -> Vec<(PathBuf, Option<String>)> {
    fn metas_in(dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        rd.filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".meta.json"))
            })
            .collect()
    }
    let mut out: Vec<(PathBuf, Option<String>)> =
        metas_in(dir).into_iter().map(|p| (p, None)).collect();
    if let Ok(runs) = std::fs::read_dir(dir.join("workflows")) {
        for run in runs
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
        {
            let run_id = run.file_name().and_then(|n| n.to_str()).map(str::to_owned);
            out.extend(metas_in(&run).into_iter().map(|p| (p, run_id.clone())));
        }
    }
    out
}

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
    if let Some(Notice {
        at,
        outcome: Some(outcome),
    }) = notice
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::tests::{FIXTURES, at, scratch};
    use std::collections::BTreeMap;
    use std::fs;

    fn meta(id: &str, parent: Option<&str>, stopped: bool) -> AgentMeta {
        AgentMeta {
            id: AgentId(id.into()),
            agent_type: "Explore".into(),
            description: SafeText::ours("x"),
            tool_use: None,
            parent: parent.map(|p| AgentId(p.into())),
            model: None,
            stopped_by_user: stopped,
            workflow_run: None,
        }
    }
    fn done(n: u32) -> Option<Notice> {
        Some(Notice {
            at: at(n),
            outcome: Some(TaskOutcome::Completed),
        })
    }
    fn horizon(n: u32) -> Horizon {
        Horizon {
            from_start: false,
            first_at: Some(at(n)),
        }
    }

    #[test]
    fn parse_meta_reads_tree_fields_and_defaults() {
        let m = parse_meta(
            AgentId("c1".into()),
            Some("wf_abc123".into()),
            br#"{"agentType":"Explore","description":"TOKEN=sk-abcdefghijklmnopqrstuvwxyz check docs","toolUseId":"tu-c1","spawnDepth":2,"parentAgentId":"a1","model":"sonnet"}"#,
        )
        .unwrap();
        assert_eq!(m.parent, Some(AgentId("a1".into())));
        assert_eq!(m.tool_use, Some(ToolUseId("tu-c1".into())));
        assert_eq!(m.workflow_run.as_deref(), Some("wf_abc123"));
        assert!(!m.stopped_by_user);
        assert_eq!(m.description.as_str(), "check docs");
        assert_eq!(
            parse_meta(AgentId("x".into()), None, b"{not json").unwrap_err(),
            Problem::Unparsable
        );
        let bare = parse_meta(AgentId("x".into()), None, b"{}").unwrap();
        assert_eq!((bare.agent_type.as_str(), bare.parent), ("", None));
    }

    #[test]
    fn agent_state_rules_in_precedence_order() {
        let m = meta("a1", None, false);
        let killed = meta("a1", None, true);
        let cases: Vec<(&str, NodeState, NodeState)> = vec![
            (
                "1 stoppedByUser beats everything",
                agent_state(
                    &killed,
                    true,
                    None,
                    Some(at(5)),
                    false,
                    None,
                    horizon(0),
                    true,
                ),
                NodeState::Killed,
            ),
            (
                "2 relaunched after a notice",
                agent_state(&m, true, done(3), Some(at(2)), true, None, horizon(0), true),
                NodeState::Running,
            ),
            (
                "2 relaunched but session dead",
                agent_state(&m, true, None, Some(at(2)), false, None, horizon(0), false),
                NodeState::Orphaned,
            ),
            (
                "3 notice newer than the tail",
                agent_state(
                    &m,
                    false,
                    done(5),
                    Some(at(4)),
                    true,
                    None,
                    horizon(0),
                    true,
                ),
                NodeState::Finished(TaskOutcome::Completed),
            ),
            (
                "3 notice with no sidecar info",
                agent_state(&m, false, done(5), None, false, None, horizon(0), true),
                NodeState::Finished(TaskOutcome::Completed),
            ),
            (
                "3 skipped when the tail is newer",
                agent_state(
                    &m,
                    false,
                    done(5),
                    Some(at(6)),
                    false,
                    None,
                    horizon(0),
                    true,
                ),
                NodeState::Running,
            ),
            (
                "3 needs an outcome",
                agent_state(
                    &m,
                    false,
                    Some(Notice {
                        at: at(5),
                        outcome: None,
                    }),
                    Some(at(4)),
                    false,
                    None,
                    horizon(0),
                    true,
                ),
                NodeState::Running,
            ),
            (
                "4 tail looks finished",
                agent_state(&m, false, None, Some(at(4)), true, None, horizon(0), true),
                NodeState::Finished(TaskOutcome::AwaitingNotice),
            ),
            (
                "5 finished parent orphans an open child",
                agent_state(
                    &m,
                    true,
                    None,
                    Some(at(4)),
                    false,
                    Some(NodeState::Finished(TaskOutcome::Completed)),
                    horizon(0),
                    true,
                ),
                NodeState::Orphaned,
            ),
            (
                "5 but a notified child stays finished",
                agent_state(
                    &m,
                    false,
                    done(5),
                    Some(at(4)),
                    false,
                    Some(NodeState::Killed),
                    horizon(0),
                    true,
                ),
                NodeState::Finished(TaskOutcome::Completed),
            ),
            (
                "6 dead session",
                agent_state(&m, false, None, Some(at(4)), false, None, horizon(0), false),
                NodeState::Orphaned,
            ),
            (
                "7 quiet before the window",
                agent_state(&m, false, None, Some(at(1)), false, None, horizon(9), true),
                NodeState::Finished(TaskOutcome::AwaitingNotice),
            ),
            (
                "8 running",
                agent_state(
                    &m,
                    false,
                    None,
                    Some(at(10)),
                    false,
                    Some(NodeState::Running),
                    horizon(9),
                    true,
                ),
                NodeState::Running,
            ),
            (
                "8 running with no sidecar yet",
                agent_state(&m, false, None, None, false, None, horizon(9), true),
                NodeState::Running,
            ),
        ];
        for (name, got, want) in cases {
            assert_eq!(got, want, "{name}");
        }
    }

    fn fixture_tree(name: &str) -> (PathBuf, Facts) {
        let dir = scratch(name);
        let main = dir.join("sess.jsonl");
        fs::copy(Path::new(FIXTURES).join("subagents/sess.jsonl"), &main).unwrap();
        let sub = dir.join("sess/subagents");
        fs::create_dir_all(&sub).unwrap();
        for e in fs::read_dir(Path::new(FIXTURES).join("subagents/sess/subagents")).unwrap() {
            let e = e.unwrap();
            fs::copy(e.path(), sub.join(e.file_name())).unwrap();
        }
        let mut facts = Facts::from_start();
        for l in fs::read(&main).unwrap().split(|&b| b == b'\n') {
            facts.apply_line(l);
        }
        (main, facts)
    }

    fn states(s: &Subagents, main: &Facts, alive: bool) -> BTreeMap<String, (NodeState, bool)> {
        s.agents(main, alive)
            .map(|(m, st, f)| (m.id.0.clone(), (st, f.is_some())))
            .collect()
    }

    #[test]
    fn refresh_tracks_running_agents_only() {
        let (main_path, main) = fixture_tree("tree");
        let mut s = Subagents::new(&main_path);
        let (spent, problems) = s.refresh(&main, true, 1 << 20);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(spent > 0);
        let got = states(&s, &main, true);
        let want: BTreeMap<String, (NodeState, bool)> = [
            ("a1", (NodeState::Running, true)),
            ("a2", (NodeState::Finished(TaskOutcome::Completed), false)),
            ("a3", (NodeState::Killed, false)),
            ("a5", (NodeState::Finished(TaskOutcome::Completed), false)),
            ("c1", (NodeState::Finished(TaskOutcome::Completed), false)),
            ("c2", (NodeState::Running, true)),
            ("c3", (NodeState::Orphaned, false)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        assert_eq!(got, want);
        let (meta, _, facts) = s
            .agents(&main, true)
            .find(|(m, _, _)| m.id.0 == "a1")
            .unwrap();
        assert_eq!(meta.parent, None);
        assert_eq!(facts.unwrap().last_tool.as_ref().unwrap().name, "Grep");
        let (spent, problems) = s.refresh(&main, true, 1 << 20);
        assert_eq!(
            (spent, problems.len()),
            (0, 0),
            "a quiet refresh opens nothing"
        );
        assert_eq!(states(&s, &main, true), want);
    }

    #[test]
    fn refresh_defers_cursors_when_the_budget_is_spent() {
        let (main_path, main) = fixture_tree("budget");
        let mut s = Subagents::new(&main_path);
        let (spent, problems) = s.refresh(&main, true, 0);
        assert_eq!(spent, 0);
        assert_eq!(
            problems.iter().filter(|p| **p == Problem::Deferred).count(),
            3,
            "a1, c1 and c2 wait"
        );
        let got = states(&s, &main, true);
        assert_eq!(got["a1"], (NodeState::Running, false));
        let (spent, problems) = s.refresh(&main, true, 1 << 20);
        assert!(spent > 0 && problems.is_empty());
        assert_eq!(states(&s, &main, true)["a1"], (NodeState::Running, true));
    }

    #[test]
    fn late_stopped_by_user_is_picked_up_without_a_relist() {
        let (main_path, main) = fixture_tree("stopped");
        let mut s = Subagents::new(&main_path);
        s.refresh(&main, true, 1 << 20);
        assert_eq!(states(&s, &main, true)["a1"].0, NodeState::Running);
        let meta_path = main_path
            .with_extension("")
            .join("subagents/agent-a1.meta.json");
        let mut text = fs::read_to_string(&meta_path).unwrap();
        text = text.trim_end().trim_end_matches('}').to_owned() + ",\"stoppedByUser\":true}\n";
        fs::write(&meta_path, text).unwrap();
        let f = fs::OpenOptions::new().write(true).open(&meta_path).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2))
            .unwrap();
        s.refresh(&main, true, 1 << 20);
        assert_eq!(states(&s, &main, true)["a1"], (NodeState::Killed, false));
    }

    #[test]
    fn dead_session_orphans_open_agents_but_keeps_finished_ones() {
        let (main_path, main) = fixture_tree("dead");
        let mut s = Subagents::new(&main_path);
        let (spent, _) = s.refresh(&main, false, 1 << 20);
        assert_eq!(spent, 0, "a dead session reads no sidecar");
        let got = states(&s, &main, false);
        assert_eq!(got["a1"].0, NodeState::Orphaned);
        assert_eq!(got["a2"].0, NodeState::Finished(TaskOutcome::Completed));
        assert_eq!(got["a3"].0, NodeState::Killed);
        assert_eq!(got["c1"].0, NodeState::Orphaned);
        assert_eq!(got["c2"].0, NodeState::Orphaned);
        assert!(
            got.values().all(|(_, open)| !open),
            "no cursors for a dead session"
        );
    }

    #[test]
    fn missing_dir_is_an_empty_set() {
        let dir = scratch("nodir");
        let main_path = dir.join("lonely.jsonl");
        fs::write(&main_path, b"").unwrap();
        let mut s = Subagents::new(&main_path);
        let main = Facts::from_start();
        assert_eq!(s.refresh(&main, true, 1 << 20), (0, Vec::new()));
        assert_eq!(s.agents(&main, true).count(), 0);
    }
}
