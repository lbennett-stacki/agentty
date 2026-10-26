use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{At, ConfigDir, Limits, PaneId, Problem, SafeText, SessionId, Window, nonempty};

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
    let w: WireTask = serde_json::from_slice(bytes).map_err(|_| Problem::Unparsable)?;
    let id = match w.id {
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        Some(n) => n.as_u64().and_then(|n| n.try_into().ok()),
        None => None,
    };
    Ok(Task {
        id: id.ok_or(Problem::MissingField("id"))?,
        subject: SafeText::redact(w.subject.as_deref().unwrap_or_default()),
        status: match w.status.as_deref().unwrap_or_default() {
            "pending" => TaskStatus::Pending,
            "in_progress" => TaskStatus::InProgress,
            "completed" => TaskStatus::Completed,
            other => TaskStatus::Other(other.to_owned()),
        },
    })
}

pub fn task_dir(dir: &ConfigDir, sid: &SessionId) -> PathBuf {
    tasks_root(dir).join(&sid.0)
}

pub fn tasks_root(dir: &ConfigDir) -> PathBuf {
    dir.0.join("tasks")
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Statusline {
    pub ctx_used_pct: Option<f64>,
    pub window: Option<u64>,
    pub cost_usd: Option<f64>,
    /// Present only for subscription accounts, after the session's first API response.
    pub limits: Option<Limits>,
    pub at: At,
}

#[derive(Deserialize)]
struct WireStatusline {
    context_window: Option<WireCtxWindow>,
    cost: Option<WireCost>,
    rate_limits: Option<WireRateLimits>,
}

#[derive(Deserialize)]
struct WireRateLimits {
    five_hour: Option<WireRateWindow>,
    seven_day: Option<WireRateWindow>,
}

#[derive(Deserialize)]
struct WireRateWindow {
    used_percentage: Option<f64>,
    /// Unix epoch seconds.
    resets_at: Option<f64>,
}

impl WireRateWindow {
    fn window(self) -> Option<Window> {
        Some(Window {
            used_pct: self.used_percentage?,
            resets_at: At((self.resets_at? * 1000.0) as i64),
        })
    }
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
    let w: WireStatusline = serde_json::from_slice(bytes).map_err(|_| Problem::Unparsable)?;
    Ok(Statusline {
        ctx_used_pct: w.context_window.as_ref().and_then(|c| c.used_percentage),
        window: w
            .context_window
            .as_ref()
            .and_then(|c| c.context_window_size),
        cost_usd: w.cost.and_then(|c| c.total_cost_usd),
        limits: w.rate_limits.map(|r| Limits {
            five_hour: r.five_hour.and_then(WireRateWindow::window),
            seven_day: r.seven_day.and_then(WireRateWindow::window),
            at: mtime,
        }),
        at: mtime,
    })
}

/// The file where Claude Code keeps an account's global config: `~/.claude.json` for the default
/// `~/.claude`, `<dir>/.claude.json` for any `CLAUDE_CONFIG_DIR`.
pub fn account_file(home: &Path, dir: &ConfigDir) -> PathBuf {
    if dir.0 == home.join(".claude") {
        home.join(".claude.json")
    } else {
        dir.0.join(".claude.json")
    }
}

// The account file also holds the account's email and org. Only the usage cache is declared, so
// nothing else is ever materialised.
#[derive(Deserialize)]
struct WireAccountFile {
    #[serde(rename = "cachedUsageUtilization")]
    usage: Option<WireUsageCache>,
}

#[derive(Deserialize)]
struct WireUsageCache {
    #[serde(rename = "fetchedAtMs")]
    fetched_at_ms: Option<i64>,
    utilization: Option<WireUtilization>,
}

#[derive(Deserialize)]
struct WireUtilization {
    five_hour: Option<WireCacheWindow>,
    seven_day: Option<WireCacheWindow>,
}

#[derive(Deserialize)]
struct WireCacheWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

impl WireCacheWindow {
    fn window(self) -> Option<Window> {
        Some(Window {
            used_pct: self.utilization?,
            resets_at: At::parse_iso(&self.resets_at?)?,
        })
    }
}

/// Reads the usage Claude Code cached the last time `/usage` fetched it. It can be days old;
/// `Limits::merge` prefers a newer statusline observation.
pub fn parse_limits_cache(bytes: &[u8]) -> Result<Option<Limits>, Problem> {
    let w: WireAccountFile = serde_json::from_slice(bytes).map_err(|_| Problem::Unparsable)?;
    Ok(w.usage.and_then(|c| {
        let u = c.utilization?;
        Some(Limits {
            five_hour: u.five_hour.and_then(WireCacheWindow::window),
            seven_day: u.seven_day.and_then(WireCacheWindow::window),
            at: At(c.fetched_at_ms?),
        })
    }))
}

pub fn statusline_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("statusline")
}

pub fn statusline_path(cache_root: &Path, sid: &SessionId) -> PathBuf {
    statusline_dir(cache_root).join(format!("{}.json", sid.0))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Team {
    pub name: SafeText,
    pub lead: Option<SessionId>,
    pub members: Vec<TeamMember>,
}

impl Team {
    /// The lead session, or a split-pane member found by its tmux pane (GROUNDING 3.11).
    pub fn includes(&self, session: &SessionId, pane: Option<&PaneId>) -> bool {
        self.lead.as_ref() == Some(session)
            || pane.is_some_and(|p| self.members.iter().any(|m| m.pane.as_ref() == Some(p)))
    }
}

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
    members: Option<Vec<WireMember>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMember {
    name: Option<String>,
    tmux_pane_id: Option<String>,
    is_active: Option<bool>,
}

pub fn parse_team(bytes: &[u8]) -> Result<Team, Problem> {
    let w: WireTeam = serde_json::from_slice(bytes).map_err(|_| Problem::Unparsable)?;
    Ok(Team {
        name: SafeText::redact(&nonempty(w.name).ok_or(Problem::MissingField("name"))?),
        lead: nonempty(w.lead_session_id).map(SessionId),
        members: w
            .members
            .into_iter()
            .flatten()
            .map(|m| TeamMember {
                name: m.name.unwrap_or_default(),
                pane: m.tmux_pane_id.as_deref().and_then(PaneId::parse),
                active: m.is_active.unwrap_or(false),
            })
            .collect(),
    })
}

pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let (dir, dot_git) = cwd
        .ancestors()
        .map(|d| (d, d.join(".git")))
        .find(|(_, g)| g.exists())?;
    let root = if dot_git.is_file() {
        linked_main(dir, &dot_git)
    } else {
        dir.to_path_buf()
    };
    Some(std::fs::canonicalize(&root).unwrap_or(root))
}

/// The main checkout for a `.git` file. `gitdir: <common>/worktrees/<name>` is a linked
/// worktree, whose common dir is `<main>/.git` (or a bare repo, which is its own main). Any
/// other gitdir (a submodule) is its own repo.
fn linked_main(dir: &Path, dot_git: &Path) -> PathBuf {
    let gitdir = std::fs::read_to_string(dot_git).ok().and_then(|s| {
        s.lines()
            .next()?
            .strip_prefix("gitdir:")
            .map(|g| dir.join(g.trim()))
    });
    let common = gitdir.as_deref().and_then(|g| {
        let worktrees = g.parent()?;
        (worktrees.file_name()? == "worktrees").then(|| worktrees.parent())?
    });
    match common {
        Some(c) if c.file_name().is_some_and(|n| n == ".git") => {
            c.parent().unwrap_or(c).to_path_buf()
        }
        Some(c) => c.to_path_buf(),
        None => dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::tests::Scratch;

    #[test]
    fn task_parses_string_or_numeric_ids_and_unknown_status() {
        let t = parse_task(br#"{"id":"3","subject":"Write the parser","description":"x","status":"in_progress","blocks":[]}"#).unwrap();
        assert_eq!(
            (t.id, t.subject.as_str(), t.status),
            (3, "Write the parser", TaskStatus::InProgress)
        );
        let t = parse_task(br#"{"id":4,"subject":"Ship","status":"deferred"}"#).unwrap();
        assert_eq!((t.id, t.status), (4, TaskStatus::Other("deferred".into())));
        assert_eq!(
            parse_task(br#"{"subject":"x"}"#),
            Err(Problem::MissingField("id"))
        );
    }

    #[test]
    fn task_subject_is_redacted() {
        let t = parse_task(
            br#"{"id":"1","subject":"Rotate ghp_0123456789abcdefghijABCDEFGHIJ0123 now"}"#,
        )
        .unwrap();
        assert_eq!(t.subject.as_str(), "Rotate … now");
    }

    #[test]
    fn statusline_takes_exact_values_and_tolerates_missing_parts() {
        let s = parse_statusline(
            br#"{"session_id":"s","context_window":{"used_percentage":12.5,"context_window_size":200000,"total_input_tokens":25000},"cost":{"total_cost_usd":0.75},"model":{"display_name":"X"}}"#,
            At(7),
        )
        .unwrap();
        assert_eq!(
            s,
            Statusline {
                ctx_used_pct: Some(12.5),
                window: Some(200000),
                cost_usd: Some(0.75),
                limits: None,
                at: At(7)
            }
        );
        let s = parse_statusline(br#"{"session_id":"s"}"#, At(8)).unwrap();
        assert_eq!(
            s,
            Statusline {
                ctx_used_pct: None,
                window: None,
                cost_usd: None,
                limits: None,
                at: At(8)
            }
        );
        assert_eq!(parse_statusline(b"{", At(9)), Err(Problem::Unparsable));
    }

    #[test]
    fn statusline_rate_limits_become_usage_observed_at_the_file_mtime() {
        let s = parse_statusline(
            br#"{"rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":1738425600},"seven_day":{"used_percentage":41.2,"resets_at":1738857600}}}"#,
            At(9),
        )
        .unwrap();
        assert_eq!(
            s.limits,
            Some(Limits {
                five_hour: Some(Window {
                    used_pct: 23.5,
                    resets_at: At(1_738_425_600_000)
                }),
                seven_day: Some(Window {
                    used_pct: 41.2,
                    resets_at: At(1_738_857_600_000)
                }),
                at: At(9)
            })
        );
        let dropped = parse_statusline(
            br#"{"rate_limits":{"seven_day":{"used_percentage":5,"resets_at":1738857600}}}"#,
            At(9),
        )
        .unwrap();
        assert_eq!(dropped.limits.unwrap().five_hour, None);
    }

    #[test]
    fn limits_cache_reads_only_the_cached_utilization() {
        // Shape captured from Claude Code 2.1.310's `.claude.json`, trimmed.
        let bytes = br#"{
            "oauthAccount": {"emailAddress": "SECRET_SENTINEL_x9"},
            "cachedUsageUtilization": {
                "fetchedAtMs": 1792863195015,
                "accountUuid": "f7f66a7a",
                "utilization": {
                    "five_hour": {"utilization": 100, "resets_at": "2026-10-24T20:29:59.584178+00:00", "limit_dollars": null},
                    "seven_day": {"utilization": 52, "resets_at": "2026-10-29T18:59:59.584203+00:00"},
                    "seven_day_opus": null
                }
            }
        }"#;
        let u = parse_limits_cache(bytes).unwrap().unwrap();
        assert_eq!(u.at, At(1_792_863_195_015));
        assert_eq!(u.five_hour.unwrap().used_pct, 100.0);
        assert_eq!(
            u.seven_day.unwrap().resets_at,
            At::parse_iso("2026-10-29T18:59:59.584203+00:00").unwrap()
        );
        assert!(!format!("{u:?}").contains("SECRET_SENTINEL_x9"));
        assert_eq!(parse_limits_cache(br#"{"projects":{}}"#), Ok(None));
        assert_eq!(parse_limits_cache(b"{"), Err(Problem::Unparsable));
    }

    #[test]
    fn the_default_account_keeps_its_file_in_home() {
        let home = Path::new("/Users/u");
        assert_eq!(
            account_file(home, &ConfigDir(home.join(".claude"))),
            home.join(".claude.json")
        );
        assert_eq!(
            account_file(home, &ConfigDir(home.join(".claude-wukong"))),
            home.join(".claude-wukong/.claude.json")
        );
    }

    #[test]
    fn team_members_keep_only_real_tmux_panes() {
        let bytes = std::fs::read(format!(
            "{}/tests/fixtures/sidecars/team_config.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let team = parse_team(&bytes).unwrap();
        assert_eq!(team.name.as_str(), "session-0a1b2c3d");
        assert_eq!(
            team.lead,
            Some(SessionId("0a1b2c3d-0000-4000-8000-000000000000".into()))
        );
        let members: Vec<_> = team
            .members
            .iter()
            .map(|m| {
                (
                    m.name.as_str(),
                    m.pane.as_ref().map(PaneId::as_str),
                    m.active,
                )
            })
            .collect();
        assert_eq!(
            members,
            [
                ("lead", None, true),
                ("tester", Some("%42"), true),
                ("helper", None, false)
            ]
        );
    }

    #[test]
    fn linked_worktrees_fold_into_the_main_repo() {
        let tmp = Scratch::new("sidecars-repo");
        let main = tmp.path().join("demo");
        tmp.mkdir("demo/.git/worktrees/fees");
        tmp.mkdir("demo/src/deep");
        tmp.mkdir("demo/.claude/worktrees/fees/src");
        let gitdir = std::fs::canonicalize(main.join(".git/worktrees/fees")).unwrap();
        tmp.write(
            "demo/.claude/worktrees/fees/.git",
            format!("gitdir: {}\n", gitdir.display()).as_bytes(),
        );
        let main_root = std::fs::canonicalize(&main).unwrap();

        assert_eq!(repo_root(&main), Some(main_root.clone()));
        assert_eq!(repo_root(&main.join("src/deep")), Some(main_root.clone()));
        assert_eq!(
            repo_root(&main.join(".claude/worktrees/fees/src")),
            Some(main_root)
        );
    }

    #[test]
    fn submodule_and_bare_layouts_resolve_to_their_own_repo() {
        let tmp = Scratch::new("sidecars-layouts");
        tmp.mkdir("super/.git/modules/lib");
        tmp.write("super/lib/.git", b"gitdir: ../.git/modules/lib\n");
        assert_eq!(
            repo_root(&tmp.path().join("super/lib")),
            std::fs::canonicalize(tmp.path().join("super/lib")).ok()
        );

        tmp.mkdir("bare.git/worktrees/wt");
        tmp.write(
            "wt/.git",
            format!(
                "gitdir: {}\n",
                tmp.path().join("bare.git/worktrees/wt").display()
            )
            .as_bytes(),
        );
        assert_eq!(
            repo_root(&tmp.path().join("wt")),
            std::fs::canonicalize(tmp.path().join("bare.git")).ok()
        );
    }

    #[test]
    fn cwd_outside_any_repo_has_no_root() {
        let tmp = Scratch::new("sidecars-norepo");
        tmp.mkdir("plain/dir");
        assert_eq!(repo_root(&tmp.path().join("plain/dir")), None);
    }
}
