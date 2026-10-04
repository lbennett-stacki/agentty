//! agentically's own state: labels, groups and anchors, in `~/.config/agentically/state.toml`.
//! The only file the app writes.
//!
//! Write discipline: several agentically processes can run at once (a persistent window and a
//! popup). Each commit is reload-if-changed -> apply one idempotent `Op` -> write
//! `state.toml.tmp-<pid>` -> fsync -> rename over `state.toml`. A reader never sees a torn file
//! (rename is atomic), and because every op is idempotent and applied to the freshest state, two
//! processes interleaving commits lose nothing except a sub-millisecond race inside
//! reload->rename, where the later writer's reload predates the earlier writer's rename
//! (accepted). Each process reloads on its own doorbell when the file's stamp moves.
//!
//! On-disk shape (version 1):
//! ```toml
//! version = 1
//! groups = ["jnj-web", "side quests"]          # display order
//! [entries."sid:91c784ff-…"]
//! label = "arch arena"
//! group = "side quests"
//! [entries."sid:91c784ff-…".anchor]
//! dir = "/Users/you/.claude"
//! pid = 41213
//! proc_start = "Sun Oct  4 21:17:02 2026"
//! [entries."job:9b3323e1"]
//! label = "fees"
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Anchor, At, ConfigDir, GroupName, Op, Pid, ProcStart, SessionId, UserKey};
use crate::transcript::FileStamp;

const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserState {
    /// Display order of user groups. A group exists while it is in this list; `SetGroup` with a
    /// new name appends. Groups with no live rows are hidden at render time, not deleted.
    pub groups: Vec<GroupName>,
    pub entries: BTreeMap<UserKey, Entry>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub label: Option<String>,
    pub group: Option<GroupName>,
    /// Only on `UserKey::Session` entries. Invariant kept by `apply`: at most one entry holds a
    /// given anchor (`Rekey` clears it on `from`, `Anchor` clears it everywhere else).
    pub anchor: Option<Anchor>,
}

impl UserState {
    /// Apply one op. Returns whether anything changed. Idempotent by construction:
    /// `apply(op); apply(op)` leaves the same state as one `apply(op)`. An entry with neither
    /// label nor group is removed (its anchor alone is not worth a table), so the file does not
    /// accumulate empty entries.
    pub fn apply(&mut self, op: &Op) -> bool {
        let before = self.clone();
        match op {
            Op::SetLabel { key, label } => {
                self.entries.entry(key.clone()).or_default().label = label.clone();
            }
            Op::SetGroup { key, group } => {
                if let Some(g) = group
                    && !self.groups.contains(g)
                {
                    self.groups.push(g.clone());
                }
                self.entries.entry(key.clone()).or_default().group = group.clone();
            }
            Op::Anchor { session, anchor } => {
                for e in self.entries.values_mut() {
                    if e.anchor.as_ref() == Some(anchor) {
                        e.anchor = None;
                    }
                }
                if let Some(e) = self.entries.get_mut(&UserKey::Session(session.clone())) {
                    e.anchor = Some(anchor.clone());
                }
            }
            Op::Rekey { from, to } => {
                let from = UserKey::Session(from.clone());
                if let Some(e) = self.entries.get(&from).cloned() {
                    self.entries.entry(UserKey::Session(to.clone())).or_insert(e);
                    if let Some(e) = self.entries.get_mut(&from) {
                        e.anchor = None;
                    }
                }
            }
        }
        self.entries.retain(|_, e| e.label.is_some() || e.group.is_some());
        *self != before
    }

    pub fn entry(&self, key: &UserKey) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// Position of a group in display order, for `GroupId::User { order }`.
    pub fn group_order(&self, name: &GroupName) -> Option<u32> {
        self.groups.iter().position(|g| g == name).map(|i| i as u32)
    }
}

/// Reconcile anchors with what is live. For each live `(anchor, sid)` (from
/// `Row::live_anchor`):
/// - an entry at `Session(sid)` whose anchor is not a live holder of `sid` -> `Op::Anchor`
///   (first labelling, or a process that resumed an old conversation);
/// - no entry at `Session(sid)` but some entry carries `anchor` -> `Op::Rekey { from, to: sid }`
///   (the process did `/clear`; the label follows, even if the dashboard was closed then).
///
/// Pure; returns an empty vec when nothing changed, so the steady state commits nothing.
pub fn rebind<'a>(
    state: &UserState,
    live: impl IntoIterator<Item = (Anchor, &'a SessionId)>,
) -> Vec<Op> {
    let live: Vec<(Anchor, &SessionId)> = live.into_iter().collect();
    let mut ops = Vec::new();
    for (anchor, sid) in &live {
        match state.entry(&UserKey::Session((*sid).clone())) {
            Some(e) => {
                // Two live holders of one sessionId: either anchor is right, so never flip-flop.
                let held = e
                    .anchor
                    .as_ref()
                    .is_some_and(|a| live.iter().any(|(la, ls)| la == a && ls == sid));
                if !held {
                    ops.push(Op::Anchor { session: (*sid).clone(), anchor: anchor.clone() });
                }
            }
            None => {
                let from = state.entries.iter().find_map(|(k, e)| match k {
                    UserKey::Session(s) if e.anchor.as_ref() == Some(anchor) => Some(s),
                    _ => None,
                });
                if let Some(from) = from {
                    ops.push(Op::Rekey { from: from.clone(), to: (*sid).clone() });
                }
            }
        }
    }
    ops
}

#[derive(Deserialize)]
struct WireVersion {
    version: u32,
}

#[derive(Serialize, Deserialize)]
struct WireState {
    version: u32,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    entries: BTreeMap<String, WireEntry>,
}

#[derive(Serialize, Deserialize)]
struct WireEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor: Option<WireAnchor>,
}

#[derive(Serialize, Deserialize)]
struct WireAnchor {
    dir: PathBuf,
    pid: u32,
    proc_start: String,
}

/// Single owner of the file within a process.
pub struct Store {
    path: PathBuf,
    stamp: Option<FileStamp>,
    state: UserState,
}

impl Store {
    /// Load, or start empty if the file is missing. A corrupt file is moved aside to
    /// `state.toml.bad-<epoch-ms>` and the app starts empty rather than refusing to run. A file
    /// from a newer version is an error: an old binary must not rewrite it.
    pub fn load(path: PathBuf) -> anyhow::Result<Store> {
        let (state, stamp) = read_state(&path)?;
        Ok(Store { path, stamp, state })
    }

    /// `~/.config/agentically/state.toml` (XDG-style even on macOS, per SPEC).
    pub fn default_path(home: &Path) -> PathBuf {
        home.join(".config/agentically/state.toml")
    }

    /// Reload if another process replaced the file. Returns whether the state changed.
    pub fn reload_if_changed(&mut self) -> anyhow::Result<bool> {
        if stamp_of(&self.path)? == self.stamp {
            return Ok(false);
        }
        let (state, stamp) = read_state(&self.path)?;
        self.stamp = stamp;
        let changed = state != self.state;
        self.state = state;
        Ok(changed)
    }

    /// reload_if_changed -> apply -> (if changed) atomic write. No write when the op is a no-op,
    /// so the steady-state `rebind` stream from several processes does not churn the file.
    pub fn commit(&mut self, op: &Op) -> anyhow::Result<()> {
        self.reload_if_changed()?;
        if !self.state.apply(op) {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Per-process tmp name: two instances sharing one tmp file could interleave bytes.
        let tmp = sibling(&self.path, &format!("tmp-{}", std::process::id()));
        let mut f = fs::File::create(&tmp)?;
        f.write_all(encode(&self.state).as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        self.stamp = stamp_of(&self.path)?;
        Ok(())
    }

    pub fn state(&self) -> &UserState {
        &self.state
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `state.toml` -> `state.toml.<suffix>`.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{suffix}"));
    path.with_file_name(name)
}

/// `None` when the file does not exist.
fn stamp_of(path: &Path) -> std::io::Result<Option<FileStamp>> {
    match fs::metadata(path) {
        Ok(m) => Ok(Some(FileStamp {
            ino: m.ino(),
            len: m.len(),
            mtime_ms: m.mtime() * 1000 + m.mtime_nsec() / 1_000_000,
        })),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The stamp is taken before the read, so a replace racing the read costs one extra reload
/// later, never a missed one.
fn read_state(path: &Path) -> anyhow::Result<(UserState, Option<FileStamp>)> {
    let stamp = stamp_of(path)?;
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((UserState::default(), None)),
        Err(e) => return Err(e.into()),
    };
    match decode(&text) {
        Ok(state) => Ok((state, stamp)),
        Err(Decode::Newer(v)) => anyhow::bail!(
            "{} has version {v}; this build understands version {VERSION} and will not overwrite it",
            path.display()
        ),
        Err(Decode::Corrupt) => {
            fs::rename(path, sibling(path, &format!("bad-{}", At::now().0)))?;
            Ok((UserState::default(), None))
        }
    }
}

#[derive(Debug, PartialEq)]
enum Decode {
    Corrupt,
    Newer(u32),
}

/// Pure deserialization, split out for round-trip tests. The version is read on its own first:
/// a newer file may not fit this build's shape, and it must be refused, not moved aside as
/// corrupt. Entry keys this build cannot decode are skipped.
fn decode(text: &str) -> Result<UserState, Decode> {
    let WireVersion { version } = toml::from_str(text).map_err(|_| Decode::Corrupt)?;
    if version > VERSION {
        return Err(Decode::Newer(version));
    }
    let wire: WireState = toml::from_str(text).map_err(|_| Decode::Corrupt)?;
    let entries = wire
        .entries
        .into_iter()
        .filter_map(|(k, e)| {
            let entry = Entry {
                label: e.label,
                group: e.group.map(GroupName),
                anchor: e.anchor.map(|a| Anchor {
                    dir: ConfigDir(a.dir),
                    pid: Pid(a.pid),
                    proc_start: ProcStart::new(&a.proc_start),
                }),
            };
            Some((UserKey::decode(&k)?, entry))
        })
        .collect();
    Ok(UserState { groups: wire.groups.into_iter().map(GroupName).collect(), entries })
}

fn encode(state: &UserState) -> String {
    let wire = WireState {
        version: VERSION,
        groups: state.groups.iter().map(|g| g.0.clone()).collect(),
        entries: state
            .entries
            .iter()
            .map(|(k, e)| {
                let entry = WireEntry {
                    label: e.label.clone(),
                    group: e.group.as_ref().map(|g| g.0.clone()),
                    anchor: e.anchor.as_ref().map(|a| WireAnchor {
                        dir: a.dir.0.clone(),
                        pid: a.pid.0,
                        proc_start: a.proc_start.as_str().to_owned(),
                    }),
                };
                (k.encode(), entry)
            })
            .collect(),
    };
    toml::to_string(&wire).expect("WireState holds only strings, ints and tables")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::JobId;

    fn sid(s: &str) -> SessionId {
        SessionId(s.into())
    }
    fn skey(s: &str) -> UserKey {
        UserKey::Session(sid(s))
    }
    fn anchor(pid: u32) -> Anchor {
        Anchor {
            dir: ConfigDir("/home/u/.claude".into()),
            pid: Pid(pid),
            proc_start: ProcStart::new("Sun Oct  4 21:17:02 2026"),
        }
    }
    fn group(g: &str) -> GroupName {
        GroupName(g.into())
    }
    fn label(key: UserKey, l: &str) -> Op {
        Op::SetLabel { key, label: Some(l.into()) }
    }

    /// A unique scratch dir under the system temp dir, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Scratch {
            let p = std::env::temp_dir()
                .join(format!("agentically-us-{}-{name}-{}", std::process::id(), At::now().0));
            fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
        fn state(&self) -> PathBuf {
            self.0.join("cfg/state.toml")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Defect: an op that is not idempotent (duplicate group in `groups`, a second apply that
    // reports a change and makes every rebind tick rewrite the file).
    #[test]
    fn every_op_applied_twice_equals_once_and_reports_no_second_change() {
        let mut base = UserState::default();
        base.apply(&label(skey("a"), "arch"));
        base.apply(&Op::SetGroup { key: skey("a"), group: Some(group("main")) });
        base.apply(&Op::Anchor { session: sid("a"), anchor: anchor(1) });
        let ops = [
            label(UserKey::Job(JobId("9b3323e1".into())), "fees"),
            Op::SetLabel { key: skey("a"), label: None },
            Op::SetGroup { key: skey("a"), group: Some(group("side")) },
            Op::SetGroup { key: skey("a"), group: None },
            Op::Anchor { session: sid("a"), anchor: anchor(2) },
            Op::Rekey { from: sid("a"), to: sid("b") },
        ];
        for op in &ops {
            let mut once = base.clone();
            assert!(once.apply(op), "first apply of {op:?} changed nothing");
            let mut twice = once.clone();
            assert!(!twice.apply(op), "second apply of {op:?} reported a change");
            assert_eq!(twice, once, "{op:?}");
        }
    }

    // Defect: a new group appended twice, or an existing group re-appended out of order.
    #[test]
    fn set_group_appends_new_names_once_in_first_use_order() {
        let mut s = UserState::default();
        s.apply(&Op::SetGroup { key: skey("a"), group: Some(group("x")) });
        s.apply(&Op::SetGroup { key: skey("b"), group: Some(group("y")) });
        s.apply(&Op::SetGroup { key: skey("c"), group: Some(group("x")) });
        assert_eq!(s.groups, vec![group("x"), group("y")]);
        assert_eq!(s.group_order(&group("y")), Some(1));
    }

    // Defect: entries emptied by clears pile up in the file forever.
    #[test]
    fn an_entry_emptied_by_clears_is_removed() {
        let mut s = UserState::default();
        s.apply(&label(skey("a"), "x"));
        s.apply(&Op::Anchor { session: sid("a"), anchor: anchor(1) });
        s.apply(&Op::SetLabel { key: skey("a"), label: None });
        assert!(s.entries.is_empty());
    }

    // Defect: Rekey overwriting a label the target already has, or leaving the anchor on both
    // entries so the next /clear is ambiguous.
    #[test]
    fn rekey_never_overwrites_the_target_and_clears_the_source_anchor() {
        let mut s = UserState::default();
        s.apply(&label(skey("a"), "old"));
        s.apply(&Op::Anchor { session: sid("a"), anchor: anchor(1) });
        s.apply(&label(skey("b"), "mine"));
        s.apply(&Op::Rekey { from: sid("a"), to: sid("b") });
        assert_eq!(s.entry(&skey("b")).unwrap().label.as_deref(), Some("mine"));
        assert_eq!(s.entry(&skey("a")).unwrap().anchor, None);
        assert_eq!(s.entry(&skey("a")).unwrap().label.as_deref(), Some("old"));
    }

    // Defect: one anchor held by two entries after an Anchor op.
    #[test]
    fn anchor_op_takes_the_anchor_from_any_other_entry() {
        let mut s = UserState::default();
        s.apply(&label(skey("a"), "a"));
        s.apply(&label(skey("b"), "b"));
        s.apply(&Op::Anchor { session: sid("a"), anchor: anchor(1) });
        s.apply(&Op::Anchor { session: sid("b"), anchor: anchor(1) });
        assert_eq!(s.entry(&skey("a")).unwrap().anchor, None);
        assert_eq!(s.entry(&skey("b")).unwrap().anchor, Some(anchor(1)));
    }

    // Defect: a label that does not follow `/clear` (new sessionId under the same process),
    // or a rebind that keeps emitting ops in the steady state.
    #[test]
    fn rebind_carries_a_label_across_clear_then_goes_quiet() {
        let mut s = UserState::default();
        s.apply(&label(skey("a"), "arch"));
        let a = sid("a");
        let ops = rebind(&s, [(anchor(7), &a)]);
        assert_eq!(ops, vec![Op::Anchor { session: sid("a"), anchor: anchor(7) }]);
        for op in &ops {
            s.apply(op);
        }
        assert_eq!(rebind(&s, [(anchor(7), &a)]), vec![]);

        let b = sid("b");
        let ops = rebind(&s, [(anchor(7), &b)]);
        assert_eq!(ops, vec![Op::Rekey { from: sid("a"), to: sid("b") }]);
        for op in &ops {
            s.apply(op);
        }
        let moved = s.entry(&skey("b")).unwrap();
        assert_eq!(moved.label.as_deref(), Some("arch"));
        assert_eq!(moved.anchor, Some(anchor(7)));
        assert_eq!(rebind(&s, [(anchor(7), &b)]), vec![]);
    }

    // Defect: rebind inventing entries for unlabelled sessions, or flip-flopping the anchor
    // between two live holders of one sessionId.
    #[test]
    fn rebind_ignores_unlabelled_sessions_and_settles_on_two_holders() {
        let mut s = UserState::default();
        let z = sid("z");
        assert_eq!(rebind(&s, [(anchor(1), &z)]), vec![]);
        s.apply(&label(skey("a"), "x"));
        let a = sid("a");
        for op in rebind(&s, [(anchor(1), &a), (anchor(2), &a)]) {
            s.apply(&op);
        }
        assert_eq!(rebind(&s, [(anchor(1), &a), (anchor(2), &a)]), vec![]);
    }

    // Defect: anchors, groups or labels lost on a write/read round trip; None written as "".
    #[test]
    fn encode_decode_round_trips_with_anchors() {
        let mut s = UserState::default();
        s.apply(&label(skey("91c7"), "arch arena"));
        s.apply(&Op::SetGroup { key: skey("91c7"), group: Some(group("side quests")) });
        s.apply(&Op::Anchor { session: sid("91c7"), anchor: anchor(41213) });
        s.apply(&label(UserKey::Job(JobId("9b3323e1".into())), "fees"));
        let text = encode(&s);
        assert!(text.contains("version = 1"), "{text}");
        assert!(!text.contains("\"\""), "{text}");
        assert_eq!(decode(&text), Ok(s));
    }

    // Defect: an old binary treating a newer file as corrupt (moving it aside) because the
    // newer shape does not parse.
    #[test]
    fn a_newer_version_is_refused_even_when_its_shape_differs() {
        assert_eq!(decode("version = 2\ngroups = 5\n"), Err(Decode::Newer(2)));
        assert_eq!(decode("groups = []\n"), Err(Decode::Corrupt));
    }

    // Defect: a commit that drops what another process committed in between (popup + window).
    #[test]
    fn two_stores_on_one_path_keep_each_others_commits() {
        let dir = Scratch::new("interleave");
        let mut a = Store::load(dir.state()).unwrap();
        let mut b = Store::load(dir.state()).unwrap();
        a.commit(&label(skey("1"), "one")).unwrap();
        b.commit(&label(skey("2"), "two")).unwrap();
        a.commit(&Op::SetGroup { key: skey("3"), group: Some(group("g")) }).unwrap();
        let fresh = Store::load(dir.state()).unwrap();
        let labels: Vec<_> = fresh.state().entries.values().map(|e| e.label.clone()).collect();
        assert_eq!(labels, vec![Some("one".into()), Some("two".into()), None]);
        assert_eq!(fresh.state().groups, vec![group("g")]);
        assert_eq!(fresh.state(), a.state());
    }

    // Defect: the steady-state rebind stream rewriting the file every refresh.
    #[test]
    fn a_no_op_commit_does_not_touch_the_file() {
        let dir = Scratch::new("noop");
        let mut s = Store::load(dir.state()).unwrap();
        s.commit(&label(skey("1"), "one")).unwrap();
        let before = stamp_of(&dir.state()).unwrap();
        s.commit(&label(skey("1"), "one")).unwrap();
        assert_eq!(stamp_of(&dir.state()).unwrap(), before);
        assert!(!s.reload_if_changed().unwrap());
    }

    // Defect: a corrupt file blocking startup, or being silently overwritten (lost forever).
    #[test]
    fn a_corrupt_file_is_moved_aside_and_the_store_starts_empty() {
        let dir = Scratch::new("corrupt");
        let cfg = dir.state().parent().unwrap().to_owned();
        fs::create_dir_all(&cfg).unwrap();
        fs::write(dir.state(), "this is = = not toml").unwrap();
        let s = Store::load(dir.state()).unwrap();
        assert_eq!(s.state(), &UserState::default());
        assert!(!dir.state().exists());
        let aside: Vec<_> = fs::read_dir(&cfg)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(aside.len(), 1);
        assert!(aside[0].starts_with("state.toml.bad-"), "{aside:?}");
        assert_eq!(fs::read_to_string(cfg.join(&aside[0])).unwrap(), "this is = = not toml");
    }

    // Defect: an old binary loading a newer file and overwriting it on the next commit.
    #[test]
    fn a_newer_file_refuses_to_load_and_is_left_in_place() {
        let dir = Scratch::new("newer");
        fs::create_dir_all(dir.state().parent().unwrap()).unwrap();
        fs::write(dir.state(), "version = 2\n").unwrap();
        assert!(Store::load(dir.state()).is_err());
        assert_eq!(fs::read_to_string(dir.state()).unwrap(), "version = 2\n");
    }
}
