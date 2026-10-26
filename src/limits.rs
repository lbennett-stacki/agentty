//! Rate-limit refusals in transcripts. When the API refuses a request because a subscription
//! window is full, Claude Code writes an assistant record with `error: "rate_limit"` and a
//! `quotaLimits` object naming the window and its reset time. That pins the window at 100% until
//! it resets, even when no session of the account is on screen to refresh the statusline tee,
//! which is exactly the account you stopped using because it is full.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{At, ConfigDir, Limits, SessionId, Window};
use crate::transcript::FileStamp;

/// How much of a transcript is read on first sight. A refusal further back than this was
/// followed by a lot of work, so its window has very likely reset.
const TAIL: u64 = 256 << 10;
/// Prompt history kept for attribution; a week of prompts is far smaller than this.
const HISTORY_TAIL: u64 = 512 << 10;
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const RESCAN_MS: i64 = 30_000;
/// Bytes one pass may read. A cold start over a busy week reads tens of MB, so it spreads over a
/// few ticks, newest transcripts first.
const PASS_BUDGET: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    FiveHour,
    SevenDay,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub session: SessionId,
    pub kind: Kind,
    pub resets_at: At,
    pub at: At,
}

#[derive(Deserialize)]
struct WireRecord {
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    error: Option<String>,
    #[serde(rename = "quotaLimits")]
    quota: Option<WireQuota>,
}

#[derive(Deserialize)]
struct WireQuota {
    status: Option<String>,
    #[serde(rename = "rateLimitType")]
    kind: Option<String>,
    /// Unix epoch seconds.
    #[serde(rename = "resetsAt")]
    resets_at: Option<f64>,
}

/// Refusals in complete transcript lines. `file_session` names the transcript, for records that
/// carry no `sessionId` of their own.
pub fn parse_refusals(chunk: &[u8], file_session: &SessionId) -> Vec<Refusal> {
    chunk
        .split(|&b| b == b'\n')
        .filter(|line| contains(line, b"\"rate_limit\""))
        .filter_map(|line| serde_json::from_slice::<WireRecord>(line).ok())
        .filter_map(|r| {
            let q = r.quota?;
            if r.error.as_deref() != Some("rate_limit") || q.status.as_deref() != Some("rejected") {
                return None;
            }
            let kind = match q.kind.as_deref()? {
                "five_hour" => Kind::FiveHour,
                "seven_day" => Kind::SevenDay,
                _ => return None,
            };
            Some(Refusal {
                session: r.session_id.map_or_else(|| file_session.clone(), SessionId),
                kind,
                resets_at: At((q.resets_at? * 1000.0) as i64),
                at: At::parse_iso(&r.timestamp?)?,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct WireHistory {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    /// Epoch milliseconds.
    timestamp: Option<i64>,
}

/// When each prompt in an account's `history.jsonl` was sent, by session.
pub fn parse_history(chunk: &[u8]) -> Vec<(SessionId, At)> {
    chunk
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<WireHistory>(line).ok())
        .filter_map(|h| Some((SessionId(h.session_id?), At(h.timestamp?))))
        .collect()
}

/// Credits each refusal still in force to the account that sent the request: the one whose
/// history has that session's latest prompt before the refusal. A transcript's location cannot
/// say, because config dirs may share `projects/` through a symlink, and a session can be resumed
/// under another account.
pub fn attribute<'a>(
    refusals: impl Iterator<Item = &'a Refusal>,
    histories: &[(&ConfigDir, &[(SessionId, At)])],
    now: At,
) -> Vec<(ConfigDir, Limits)> {
    let mut newest: BTreeMap<(ConfigDir, Kind), &Refusal> = BTreeMap::new();
    for r in refusals.filter(|r| r.resets_at > now) {
        let owner = histories
            .iter()
            .flat_map(|(dir, prompts)| {
                prompts
                    .iter()
                    .filter(|(s, at)| s == &r.session && *at <= r.at)
                    .map(move |(_, at)| (*at, *dir))
            })
            .max();
        if let Some((_, dir)) = owner {
            let slot = newest.entry((dir.clone(), r.kind)).or_insert(r);
            if r.at > slot.at {
                *slot = r;
            }
        }
    }
    let mut out: BTreeMap<ConfigDir, Limits> = BTreeMap::new();
    for ((dir, kind), r) in newest {
        let full = Some(Window {
            used_pct: 100.0,
            resets_at: r.resets_at,
        });
        let l = out.entry(dir).or_insert(Limits {
            five_hour: None,
            seven_day: None,
            at: r.at,
        });
        match kind {
            Kind::FiveHour => l.five_hour = full,
            Kind::SevenDay => l.seven_day = full,
        }
        l.at = l.at.max(r.at);
    }
    out.into_iter().collect()
}

struct Cursor {
    stamp: FileStamp,
    offset: u64,
    refusals: Vec<Refusal>,
}

/// Follows every transcript written in the last week for refusals, and each account's prompt
/// history to attribute them.
#[derive(Default)]
pub struct RefusalScan {
    files: HashMap<PathBuf, Cursor>,
    histories: HashMap<ConfigDir, (FileStamp, Vec<(SessionId, At)>)>,
    last_pass: Option<At>,
    unfinished: bool,
}

impl RefusalScan {
    /// Runs a pass every 30 s, or on every call while the last pass ran out of budget.
    pub fn refresh(&mut self, dirs: &[ConfigDir], now: At) {
        if !self.unfinished
            && self
                .last_pass
                .is_some_and(|t| now.millis_since(t) < RESCAN_MS)
        {
            return;
        }
        self.last_pass = Some(now);
        self.unfinished = false;
        let recent = recent_transcripts(dirs, now);
        let keep: HashSet<&PathBuf> = recent.iter().map(|(p, _)| p).collect();
        self.files.retain(|p, _| keep.contains(p));
        let mut budget = PASS_BUDGET;
        for (path, stamp) in &recent {
            let prev = self.files.get(path);
            if prev.is_some_and(|c| c.stamp == *stamp) {
                continue;
            }
            let (from, mut refusals) = match prev {
                Some(c) if c.stamp.ino == stamp.ino && stamp.len >= c.offset => {
                    (c.offset, c.refusals.clone())
                }
                _ => (stamp.len.saturating_sub(TAIL), Vec::new()),
            };
            let want = stamp.len - from;
            if want > budget {
                self.unfinished = true;
                break;
            }
            budget -= want;
            let Ok(chunk) = read_range(path, from, stamp.len) else {
                continue;
            };
            let complete = chunk.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            let session = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map_or_else(|| SessionId(String::new()), |s| SessionId(s.into()));
            refusals.extend(parse_refusals(&chunk[..complete], &session));
            refusals.retain(|r| r.resets_at > now);
            self.files.insert(
                path.clone(),
                Cursor {
                    stamp: *stamp,
                    offset: from + complete as u64,
                    refusals,
                },
            );
        }
        self.refresh_histories(dirs);
    }

    fn refresh_histories(&mut self, dirs: &[ConfigDir]) {
        let mut next = HashMap::new();
        for dir in dirs {
            let path = dir.0.join("history.jsonl");
            let Ok(stamp) = FileStamp::of(&path) else {
                continue;
            };
            let entry = match self.histories.remove(dir) {
                Some(kept) if kept.0 == stamp => kept,
                _ => {
                    let from = stamp.len.saturating_sub(HISTORY_TAIL);
                    match read_range(&path, from, stamp.len) {
                        Ok(chunk) => (stamp, parse_history(&chunk)),
                        Err(_) => continue,
                    }
                }
            };
            next.insert(dir.clone(), entry);
        }
        self.histories = next;
    }

    /// Each account's windows that a refusal shows full and not yet reset.
    pub fn limits(&self, now: At) -> Vec<(ConfigDir, Limits)> {
        let histories: Vec<(&ConfigDir, &[(SessionId, At)])> = self
            .histories
            .iter()
            .map(|(d, (_, h))| (d, h.as_slice()))
            .collect();
        attribute(
            self.files.values().flat_map(|c| &c.refusals),
            &histories,
            now,
        )
    }
}

/// Every transcript modified in the last week, newest first, each file once even when config
/// dirs share `projects/`.
fn recent_transcripts(dirs: &[ConfigDir], now: At) -> Vec<(PathBuf, FileStamp)> {
    let roots: HashSet<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::canonicalize(d.0.join("projects")).ok())
        .collect();
    let mut out = Vec::new();
    for root in roots {
        let Ok(projects) = std::fs::read_dir(&root) else {
            continue;
        };
        for project in projects.flatten() {
            let Ok(files) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                if let Ok(stamp) = FileStamp::of(&path)
                    && now.millis_since(At(stamp.mtime_ms)) < WEEK_MS
                {
                    out.push((path, stamp));
                }
            }
        }
    }
    out.sort_by_key(|(_, s)| std::cmp::Reverse(s.mtime_ms));
    out
}

fn read_range(path: &Path, from: u64, to: u64) -> std::io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut buf = vec![0; usize::try_from(to - from).unwrap_or(0)];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shapes captured from Claude Code 2.1.310 transcripts and history, trimmed.
    const REFUSAL: &str = r#"{"type":"assistant","timestamp":"2026-10-25T13:36:50.656Z","sessionId":"76e65cc7","error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,"quotaLimits":{"status":"rejected","resetsAt":1793300400,"rateLimitType":"seven_day","overageStatus":"rejected","isUsingOverage":false},"message":{"content":[{"type":"text","text":"You've hit your limit"}]}}"#;

    fn sid(s: &str) -> SessionId {
        SessionId(s.into())
    }

    fn iso(s: &str) -> At {
        At::parse_iso(s).unwrap()
    }

    #[test]
    fn a_refusal_names_its_window_reset_and_time() {
        let chunk = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"why rate_limit?\"}}}}\n{REFUSAL}\n"
        );
        assert_eq!(
            parse_refusals(chunk.as_bytes(), &sid("file")),
            [Refusal {
                session: sid("76e65cc7"),
                kind: Kind::SevenDay,
                resets_at: At(1_793_300_400_000),
                at: iso("2026-10-25T13:36:50.656Z"),
            }]
        );
    }

    #[test]
    fn only_rejections_of_known_windows_count() {
        let allowed = REFUSAL.replace("\"rejected\",\"resetsAt\"", "\"allowed\",\"resetsAt\"");
        let other = REFUSAL.replace("seven_day", "opus_weekly");
        let server = REFUSAL.replace("\"error\":\"rate_limit\"", "\"error\":\"server_error\"");
        for line in [allowed, other, server] {
            assert_eq!(parse_refusals(line.as_bytes(), &sid("f")), []);
        }
        let no_sid = REFUSAL.replace("\"sessionId\":\"76e65cc7\",", "");
        assert_eq!(
            parse_refusals(no_sid.as_bytes(), &sid("f"))[0].session,
            sid("f")
        );
    }

    #[test]
    fn history_gives_each_prompt_its_session_and_time() {
        let chunk = br#"{"display":"go","pastedContents":{},"timestamp":1793023309344,"project":"/w","sessionId":"76e65cc7"}
{"display":"half a line"#;
        assert_eq!(
            parse_history(chunk),
            [(sid("76e65cc7"), At(1_793_023_309_344))]
        );
    }

    #[test]
    fn a_refusal_goes_to_the_account_that_sent_the_last_prompt_before_it() {
        let personal = ConfigDir("/h/.claude-personal".into());
        let wukong = ConfigDir("/h/.claude-wukong".into());
        let r = |kind, at: &str, resets: &str| Refusal {
            session: sid("s1"),
            kind,
            resets_at: iso(resets),
            at: iso(at),
        };
        let refusals = [
            r(
                Kind::SevenDay,
                "2026-10-25T13:36:00Z",
                "2026-10-29T19:00:00Z",
            ),
            // After the resume on wukong, a 5-hour refusal belongs to wukong.
            r(
                Kind::FiveHour,
                "2026-10-26T10:00:00Z",
                "2026-10-26T14:00:00Z",
            ),
            // Already reset by `now`: dropped.
            r(
                Kind::FiveHour,
                "2026-10-24T17:00:00Z",
                "2026-10-24T20:30:00Z",
            ),
        ];
        let personal_prompts = [(sid("s1"), iso("2026-10-25T13:30:00Z"))];
        let wukong_prompts = [(sid("s1"), iso("2026-10-26T09:59:00Z"))];
        let now = iso("2026-10-26T12:00:00Z");
        let got = attribute(
            refusals.iter(),
            &[(&personal, &personal_prompts), (&wukong, &wukong_prompts)],
            now,
        );
        assert_eq!(
            got,
            [
                (
                    personal,
                    Limits {
                        five_hour: None,
                        seven_day: Some(Window {
                            used_pct: 100.0,
                            resets_at: iso("2026-10-29T19:00:00Z")
                        }),
                        at: iso("2026-10-25T13:36:00Z"),
                    }
                ),
                (
                    wukong,
                    Limits {
                        five_hour: Some(Window {
                            used_pct: 100.0,
                            resets_at: iso("2026-10-26T14:00:00Z")
                        }),
                        seven_day: None,
                        at: iso("2026-10-26T10:00:00Z"),
                    }
                ),
            ]
        );
        // A session no history knows is not guessed.
        assert_eq!(attribute(refusals.iter(), &[], now), []);
    }

    #[test]
    fn the_scan_reads_new_bytes_once_and_finds_refusals_across_shared_dirs() {
        let root = std::env::temp_dir().join(format!("agentty-limits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let personal = root.join(".claude-personal");
        let project = personal.join("projects/-w");
        std::fs::create_dir_all(&project).unwrap();
        let wukong = root.join(".claude-wukong");
        std::fs::create_dir_all(&wukong).unwrap();
        std::os::unix::fs::symlink(personal.join("projects"), wukong.join("projects")).unwrap();
        std::fs::write(
            personal.join("history.jsonl"),
            r#"{"timestamp":1792935000000,"sessionId":"76e65cc7"}
"#,
        )
        .unwrap();
        let transcript = project.join("76e65cc7.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        let dirs = [ConfigDir(personal.clone()), ConfigDir(wukong)];
        let now = iso("2026-10-26T12:00:00Z");
        let mut scan = RefusalScan::default();
        scan.refresh(&dirs, now);
        assert_eq!(scan.limits(now), []);
        assert_eq!(scan.files.len(), 1, "the shared projects dir is read once");

        // Append a refusal plus half a line; the next pass reads only the appended bytes.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        std::io::Write::write_all(&mut f, format!("{REFUSAL}\n{{\"type\":").as_bytes()).unwrap();
        scan.refresh(&dirs, At(now.0 + RESCAN_MS));
        let got = scan.limits(now);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, ConfigDir(personal));
        assert_eq!(got[0].1.seven_day.unwrap().used_pct, 100.0);
        let c = scan.files.values().next().unwrap();
        assert_eq!(c.offset, c.stamp.len - "{\"type\":".len() as u64);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
