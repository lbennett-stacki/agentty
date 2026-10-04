//! View state, the pure layout, the pure key reducer, and the renderer.
//!
//! `layout` turns (snapshot, user state, app) into plain `Line` data: grouping, sorting,
//! filtering, tree flattening and cursor placement are all decided here, with no ratatui types,
//! so they are tested by comparing values. `update` never does IO: it returns `Effect`s.
//! `render` never decides anything: it maps `Line`s to widgets.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;

use crate::model::{
    At, ConfigDir, Effect, GroupId, GroupName, Meta, NodeKind, NodeState, Outcome, RowKey,
    SafeText, Snapshot, Status, TaskId, UserKey,
};
use crate::user_state::UserState;

/// Everything the UI remembers between frames. The cursor and expansion are keyed by `RowKey`,
/// not by index, so a re-sort (rows move when their status changes) never moves the cursor to a
/// different session.
#[derive(Debug)]
pub struct App {
    pub popup: bool,
    cursor: Option<Target>,
    expanded: HashSet<RowKey>,
    detail: bool,
    /// The committed filter (`/` then Enter). Empty means none.
    filter: String,
    mode: Mode,
    flash: Option<Flash>,
}

/// Something the cursor can rest on. Headers and finished-counts are not selectable.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Row(RowKey),
    Node { row: RowKey, id: TaskId },
}

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    /// `/`: typing narrows rows live (title, label, branch, cwd, group); Enter keeps it, Esc
    /// clears it.
    Filter { input: String },
    /// `r`: edit the label; empty input clears it.
    Label { key: UserKey, input: String },
    /// `m`: pick an existing group (j/k + Enter), or type a new name; `-` returns to auto.
    Move { key: UserKey, input: String, choice: usize, options: Vec<GroupName> },
    /// `n`: optional name for the new session; Enter with empty input launches without `--name`.
    NewSession { dir: ConfigDir, cwd: std::path::PathBuf, group: Option<GroupName>, input: String },
    /// `?`: key map and diagnostics.
    Help,
}

#[derive(Debug)]
struct Flash {
    text: String,
    until: At,
}

/// Abstract key, so the reducer and its tests do not depend on crossterm's event type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Tab,
    Backspace,
    Space,
}

/// crossterm -> `Key`. `None` for keys the UI ignores (modifiers alone, function keys).
pub fn key_of(ev: &KeyEvent) -> Option<Key> {
    todo!()
}

/// Input to the reducer: a key, or the result of an effect the shell ran.
#[derive(Debug)]
pub enum Input {
    Key(Key),
    Done(Outcome),
}

impl App {
    pub fn new(popup: bool) -> App {
        todo!()
    }
}

/// The key handler. Pure: (app, snapshot, user state, input) -> mutated app + effects. It calls
/// `layout` to know what is under the cursor.
///
/// Normal mode:
/// - j/k/↓/↑ move over selectable lines (rows and expanded tree nodes; a node is selectable
///   only for reading, Enter on one opens its row); g/G top/bottom.
/// - l/→/space expand; h/← collapse (or jump to the parent row from a tree node).
/// - Enter: `row.open` -> Jump / Attach, or a flash for Unreachable.
/// - tab/p detail pane; r label; m move; n new session (row's dir, cwd, group; prompts for an
///   optional name); / filter; ? help; q/Esc quit (Esc first clears an active filter).
/// Outcomes: Jumped/Attached in popup mode -> [Quit]; Spawned{session, group: Some} ->
/// [Commit(SetGroup{key: Session(session)})]; Failed(msg) -> flash.
/// Cursor stability: if the cursor's target vanished from `layout`, the nearest selectable
/// line takes its position.
pub fn update(
    app: &mut App,
    snap: &Snapshot,
    user: &UserState,
    input: Input,
    now: At,
) -> Vec<Effect> {
    todo!()
}

/// One visible line, as plain data. Elapsed times are left as `since` so the renderer formats
/// them against its own clock and the layout stays clock-free.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    Header { group: GroupId, title: String, count: usize, needs_you: usize },
    Row {
        key: RowKey,
        status: Status,
        /// User label from state.toml; rendered before the title when set.
        label: Option<String>,
        title: SafeText,
        meta: Meta,
        now: SafeText,
        since: At,
        expanded: bool,
    },
    Node {
        row: RowKey,
        id: TaskId,
        kind: NodeKind,
        state: NodeState,
        label: SafeText,
        now: Option<SafeText>,
        since: At,
        /// Tree-drawing prefix (`├─`, `│  └─`).
        prefix: String,
    },
    Finished { row: RowKey, count: u16, prefix: String },
}

impl Line {
    pub fn target(&self) -> Option<Target> {
        match self {
            Line::Row { key, .. } => Some(Target::Row(key.clone())),
            Line::Node { row, id, .. } => Some(Target::Node { row: row.clone(), id: id.clone() }),
            Line::Header { .. } | Line::Finished { .. } => None,
        }
    }
}

/// The one layout function. Pure.
/// - group each row: `user.entry(row.user_key).group` as `GroupId::User { order }`, else
///   `GroupId::Auto(row.auto_group)`; groups sort by `GroupId`'s derived `Ord`;
/// - within a group sort by `model::row_order`;
/// - filter (committed `app.filter`, or the live `Mode::Filter` input) matches title, label,
///   now-line, branch, cwd and group name, case-insensitively;
/// - expanded rows get `Node` lines depth-first with tree prefixes, and one `Finished` line per
///   level with a non-zero count;
/// - empty user groups are omitted; `AutoGroup::Finished` is last by construction.
pub fn layout(snap: &Snapshot, user: &UserState, app: &App) -> Vec<Line> {
    todo!()
}

/// Draw the frame: header (counts, `⚠ n` when diagnostics exist), the `layout` lines (two
/// terminal lines per `Line::Row`), optional detail pane on the right (or below when narrower
/// than 120 cols), footer with the mode's prompt or the flash. Colours: `Color::Reset` fg/bg;
/// accents red (NeedsYou), green (Working), yellow (Parked), default (Idle, `?` glyph for
/// asked_question), dim (Exited, line 2, finished counts).
pub fn render(frame: &mut Frame, app: &App, snap: &Snapshot, user: &UserState, now: At) {
    todo!()
}

/// Status glyph for line 1: `▲` NeedsYou, `●` Working, `◐` Parked, `?` Idle asked_question,
/// `○` Idle, `·` Exited.
pub fn glyph(status: &Status) -> &'static str {
    todo!()
}

/// "42s", "3m", "1h", "2d". Pure; used for time-in-status and tree ages.
pub fn elapsed(since: At, now: At) -> String {
    todo!()
}
