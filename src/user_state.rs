//! agentically's own state: labels, groups and anchors, in `~/.config/agentically/state.toml`.
//! The only file the app writes.
//!
//! Write discipline: several agentically processes can run at once (a persistent window and a
//! popup). Each commit is reload-if-changed -> apply one idempotent `Op` -> write `state.toml.tmp`
//! -> fsync -> rename over `state.toml`. A reader never sees a torn file (rename is atomic), and
//! because every op is idempotent and applied to the freshest state, two processes interleaving
//! commits lose nothing except a sub-millisecond race inside reload->rename, where the later
//! writer's reload predates the earlier writer's rename (accepted). Each process reloads on its
//! own doorbell when the file's stamp moves.
//!
//! On-disk shape (version 1):
//! ```toml
//! version = 1
//! groups = ["jnj-web", "side quests"]          # display order
//! [entries."sid:91c784ff-…"]
//! label = "arch arena"
//! group = "side quests"
//! [entries."sid:91c784ff-…".anchor]
//! dir = "/Users/luke/.claude"
//! pid = 41213
//! proc_start = "Sun Oct  4 21:17:02 2026"
//! [entries."job:9b3323e1"]
//! label = "fees"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Anchor, GroupName, Op, SessionId, UserKey};
use crate::transcript::FileStamp;

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
    /// given anchor (`Rekey` clears it on `from`).
    pub anchor: Option<Anchor>,
}

impl UserState {
    /// Apply one op. Returns whether anything changed. Idempotent by construction:
    /// `apply(op); apply(op)` leaves the same state as one `apply(op)`. An entry with neither
    /// label nor group is removed (its anchor alone is not worth a table), so the file does not
    /// accumulate empty entries.
    pub fn apply(&mut self, op: &Op) -> bool {
        todo!()
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
/// - an entry at `Session(sid)` whose anchor differs -> `Op::Anchor` (first labelling, or a
///   process that resumed an old conversation);
/// - no entry at `Session(sid)` but some entry carries `anchor` -> `Op::Rekey { from, to: sid }`
///   (the process did `/clear`; the label follows, even if the dashboard was closed then).
/// Pure; returns an empty vec when nothing changed, so the steady state commits nothing.
pub fn rebind<'a>(
    state: &UserState,
    live: impl IntoIterator<Item = (Anchor, &'a SessionId)>,
) -> Vec<Op> {
    todo!()
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
    label: Option<String>,
    group: Option<String>,
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
    /// `state.toml.bad-<epoch>` and the app starts empty rather than refusing to run.
    pub fn load(path: PathBuf) -> anyhow::Result<Store> {
        todo!()
    }

    /// `~/.config/agentically/state.toml` (XDG-style even on macOS, per SPEC).
    pub fn default_path(home: &Path) -> PathBuf {
        home.join(".config/agentically/state.toml")
    }

    /// Reload if another process replaced the file. Returns whether the state changed.
    pub fn reload_if_changed(&mut self) -> anyhow::Result<bool> {
        todo!()
    }

    /// reload_if_changed -> apply -> (if changed) atomic write. No write when the op is a no-op,
    /// so the steady-state `rebind` stream from several processes does not churn the file.
    pub fn commit(&mut self, op: &Op) -> anyhow::Result<()> {
        todo!()
    }

    pub fn state(&self) -> &UserState {
        &self.state
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Pure (de)serialization, split out for round-trip tests. Unknown `version` > 1 refuses to
/// load (returns Err) so an older binary never rewrites a newer file and drops its fields.
fn decode(text: &str) -> anyhow::Result<UserState> {
    todo!()
}

fn encode(state: &UserState) -> String {
    todo!()
}
