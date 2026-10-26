use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{Config, Workspace};
use crate::derive::roomiest;
use crate::model::{
    At, AutoGroup, ConfigDir, Cost, Ctx, Effect, GroupId, GroupName, Meta, NodeKind, NodeState, Op,
    OpenTarget, Outcome, Problem, Rank, Row, RowKey, SafeText, Snapshot, Status, TaskId, Tree,
    UserKey, Window, row_order,
};
use crate::skills::Inventory;
use crate::user_state::UserState;

mod skills_view;
use skills_view::SkillsView;

const FLASH_MS: i64 = 4000;
/// The list's width when the details pane sits beside it.
const LIST_WIDTH: u16 = 56;
/// The selected row's background, from the 256-colour palette so it follows the terminal theme.
const BAND: Color = Color::Indexed(237);
/// The status column: glyph, space, and the longest status word.
const STATUS_WIDTH: usize = 13;

#[derive(Debug)]
pub struct App {
    pub popup: bool,
    cursor: Option<Target>,
    last_index: usize,
    expanded: HashSet<RowKey>,
    detail: bool,
    filter: String,
    mode: Mode,
    flash: Option<Flash>,
    scope: Scope,
    /// The skills view, on screen in place of the session list while `Some`.
    skills: Option<SkillsView>,
}

#[derive(Debug, Default)]
struct Scope {
    config: Config,
    /// The workspace on screen, or every session when `None`.
    current: Option<usize>,
    /// Where agentty started: `n` starts here when no row is selected.
    cwd: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Row(RowKey),
    Node { row: RowKey, id: TaskId },
}

impl Target {
    fn row(&self) -> &RowKey {
        match self {
            Target::Row(k) | Target::Node { row: k, .. } => k,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    Filter {
        input: String,
    },
    Label {
        key: UserKey,
        input: String,
    },
    Move {
        key: UserKey,
        input: String,
        choice: usize,
        options: Vec<GroupName>,
    },
    NewSession {
        accounts: Vec<ConfigDir>,
        choice: usize,
        cwd: PathBuf,
        group: Option<GroupName>,
        input: String,
        /// The first prompt, for an agent started from the skills view.
        prompt: Option<String>,
    },
    Help,
}

#[derive(Debug)]
struct Flash {
    text: String,
    until: At,
}

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

pub fn key_of(ev: &KeyEvent) -> Option<Key> {
    if ev.kind != KeyEventKind::Press
        || ev
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    Some(match ev.code {
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        _ => return None,
    })
}

#[derive(Debug)]
pub enum Input {
    Key(Key),
    Done(Outcome),
}

impl App {
    pub fn new(popup: bool) -> App {
        App {
            popup,
            cursor: None,
            last_index: 0,
            expanded: HashSet::new(),
            detail: false,
            filter: String::new(),
            mode: Mode::Normal,
            flash: None,
            scope: Scope::default(),
            skills: None,
        }
    }

    pub fn with_skills(mut self, inv: Inventory) -> App {
        self.skills = Some(SkillsView::new(inv));
        self
    }

    pub fn scoped(mut self, config: Config, current: Option<usize>, cwd: PathBuf) -> App {
        self.scope = Scope {
            config,
            current,
            cwd,
        };
        self
    }

    fn workspace(&self) -> Option<&Workspace> {
        self.scope.current.map(|i| &self.scope.config.workspaces[i])
    }

    fn in_scope(&self, row: &Row) -> bool {
        self.workspace()
            .is_none_or(|w| row.detail.cwd.starts_with(&w.root))
    }

    fn start_dir(&self, w: &Workspace) -> PathBuf {
        if self.scope.cwd.starts_with(&w.root) {
            self.scope.cwd.clone()
        } else {
            w.root.clone()
        }
    }

    fn filter_text(&self) -> &str {
        match &self.mode {
            Mode::Filter { input } => input,
            _ => &self.filter,
        }
    }

    fn place(&mut self, lines: &[Line], i: usize) {
        self.cursor = lines[i].target();
        self.last_index = i;
    }

    fn flash(&mut self, text: String, now: At) {
        self.flash = Some(Flash {
            text,
            until: At(now.0 + FLASH_MS),
        });
    }
}

pub fn update(
    app: &mut App,
    snap: &Snapshot,
    user: &UserState,
    input: Input,
    now: At,
) -> Vec<Effect> {
    let key = match input {
        Input::Done(outcome) => return on_outcome(app, outcome, now),
        Input::Key(key) => key,
    };
    let lines = layout(snap, user, app);
    if let Some(i) = selected(&lines, app) {
        app.place(&lines, i);
    }
    match std::mem::replace(&mut app.mode, Mode::Normal) {
        Mode::Normal if app.skills.is_some() => skills_view::keys(app, snap, user, key, now),
        Mode::Normal => normal(app, snap, user, &lines, key, now),
        Mode::Filter { mut input } => {
            match key {
                Key::Enter => app.filter = input,
                Key::Esc => app.filter.clear(),
                Key::Up | Key::Down => {
                    step(app, &lines, key == Key::Down);
                    app.mode = Mode::Filter { input };
                }
                _ => {
                    edit(&mut input, key);
                    app.mode = Mode::Filter { input };
                }
            }
            vec![]
        }
        Mode::Label {
            key: user_key,
            mut input,
        } => match key {
            Key::Enter => vec![Effect::Commit(Op::SetLabel {
                key: user_key,
                label: non_empty(&input),
            })],
            Key::Esc => vec![],
            _ => {
                edit(&mut input, key);
                app.mode = Mode::Label {
                    key: user_key,
                    input,
                };
                vec![]
            }
        },
        Mode::Move {
            key: user_key,
            mut input,
            mut choice,
            options,
        } => {
            let picks = candidates(&input, &options);
            match key {
                Key::Esc => return vec![],
                Key::Char('-') if input.is_empty() => {
                    return vec![Effect::Commit(Op::SetGroup {
                        key: user_key,
                        group: None,
                    })];
                }
                Key::Enter => {
                    if let Some(g) = picks.get(choice) {
                        return vec![Effect::Commit(Op::SetGroup {
                            key: user_key,
                            group: Some(g.clone()),
                        })];
                    }
                }
                Key::Up => choice = choice.saturating_sub(1),
                Key::Down | Key::Tab => choice = (choice + 1).min(picks.len().saturating_sub(1)),
                _ => {
                    edit(&mut input, key);
                    choice = 0;
                }
            }
            app.mode = Mode::Move {
                key: user_key,
                input,
                choice,
                options,
            };
            vec![]
        }
        Mode::NewSession {
            accounts,
            mut choice,
            cwd,
            group,
            mut input,
            prompt,
        } => match key {
            Key::Enter => vec![Effect::NewSession {
                dir: accounts[choice].clone(),
                cwd,
                group,
                name: non_empty(&input),
                prompt,
            }],
            Key::Esc => vec![],
            _ => {
                if key == Key::Tab {
                    choice = (choice + 1) % accounts.len();
                } else {
                    edit(&mut input, key);
                }
                app.mode = Mode::NewSession {
                    accounts,
                    choice,
                    cwd,
                    group,
                    input,
                    prompt,
                };
                vec![]
            }
        },
        Mode::Help => vec![],
    }
}

fn normal(
    app: &mut App,
    snap: &Snapshot,
    user: &UserState,
    lines: &[Line],
    key: Key,
    now: At,
) -> Vec<Effect> {
    let row = app
        .cursor
        .as_ref()
        .and_then(|t| snap.rows.iter().find(|r| &r.key == t.row()));
    let selectable: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].target().is_some())
        .collect();
    match key {
        Key::Char('j') | Key::Down => step(app, lines, true),
        Key::Char('k') | Key::Up => step(app, lines, false),
        Key::Char('g') => {
            if let Some(&i) = selectable.first() {
                app.place(lines, i);
            }
        }
        Key::Char('G') => {
            if let Some(&i) = selectable.last() {
                app.place(lines, i);
            }
        }
        Key::Char('l') | Key::Right | Key::Space => {
            if let (Some(Target::Row(k)), Some(r)) = (&app.cursor, row)
                && !r.tree.is_empty()
            {
                app.expanded.insert(k.clone());
            }
        }
        Key::Char('h') | Key::Left => match app.cursor.clone() {
            Some(Target::Node { row, .. }) => app.cursor = Some(Target::Row(row)),
            Some(Target::Row(k)) => {
                app.expanded.remove(&k);
            }
            None => {}
        },
        Key::Enter => {
            if let Some(r) = row {
                return open(app, r, now);
            }
        }
        Key::Tab | Key::Char('p') => app.detail = !app.detail,
        Key::Char('r') => {
            if let Some(r) = row {
                let input = user
                    .entry(&r.user_key)
                    .and_then(|e| e.label.clone())
                    .unwrap_or_default();
                app.mode = Mode::Label {
                    key: r.user_key.clone(),
                    input,
                };
            }
        }
        Key::Char('m') => {
            if let Some(r) = row {
                let current = user_group(user, r);
                let choice = current
                    .and_then(|g| user.groups.iter().position(|o| o == g))
                    .unwrap_or(0);
                app.mode = Mode::Move {
                    key: r.user_key.clone(),
                    input: String::new(),
                    choice,
                    options: user.groups.clone(),
                };
            }
        }
        Key::Char('n') => {
            let (accounts, cwd) = match (app.workspace(), row) {
                (Some(w), _) => (
                    w.accounts.clone(),
                    row.map_or_else(|| app.start_dir(w), |r| r.detail.cwd.clone()),
                ),
                (None, Some(r)) => (vec![row_dir(&r.key).clone()], r.detail.cwd.clone()),
                (None, None) => return vec![],
            };
            app.mode = Mode::NewSession {
                choice: roomiest(&accounts, &snap.limits, now),
                accounts,
                cwd,
                group: row.and_then(|r| user_group(user, r).cloned()),
                input: String::new(),
                prompt: None,
            };
        }
        Key::Char('s') => {
            let accounts: Vec<ConfigDir> = app
                .scope
                .config
                .accounts
                .iter()
                .map(|a| a.dir.clone())
                .collect();
            if accounts.is_empty() {
                app.flash(
                    "no accounts: add [accounts] to ~/.config/agentty/config.toml".into(),
                    now,
                );
            } else {
                return vec![Effect::ScanSkills(accounts)];
            }
        }
        Key::Char('w') => {
            let n = app.scope.config.workspaces.len();
            if n == 0 {
                app.flash(
                    "no workspaces: add them to ~/.config/agentty/config.toml".into(),
                    now,
                );
            } else {
                app.scope.current = match app.scope.current {
                    None => Some(0),
                    Some(i) if i + 1 < n => Some(i + 1),
                    Some(_) => None,
                };
            }
        }
        Key::Char('/') => {
            app.mode = Mode::Filter {
                input: app.filter.clone(),
            }
        }
        Key::Char('?') => app.mode = Mode::Help,
        Key::Char('q') => return vec![Effect::Quit],
        Key::Esc => {
            if app.filter.is_empty() {
                return vec![Effect::Quit];
            }
            app.filter.clear();
        }
        _ => {}
    }
    vec![]
}

fn on_outcome(app: &mut App, outcome: Outcome, now: At) -> Vec<Effect> {
    let quit = app.popup.then_some(Effect::Quit);
    match outcome {
        Outcome::Jumped => quit.into_iter().collect(),
        Outcome::Spawned { dir, job, group } => group
            .map(|g| {
                Effect::Commit(Op::SetGroup {
                    key: UserKey::Job(job.clone()),
                    group: Some(g),
                })
            })
            .into_iter()
            .chain([Effect::Attach { dir, job }])
            .collect(),
        Outcome::Skills(inv) => {
            app.skills = Some(match app.skills.take() {
                Some(view) => view.rescanned(inv),
                None => SkillsView::new(inv),
            });
            vec![]
        }
        // Back from a share-skill agent: show what it changed.
        Outcome::Attached => match &app.skills {
            Some(view) => vec![Effect::ScanSkills(view.inv.accounts.clone())],
            None => vec![],
        },
        Outcome::Committed => vec![],
        Outcome::Failed(msg) => {
            app.flash(msg, now);
            vec![]
        }
    }
}

fn open(app: &mut App, row: &Row, now: At) -> Vec<Effect> {
    match &row.open {
        OpenTarget::Pane(p) => vec![Effect::Jump(p.clone())],
        OpenTarget::Attach { dir, job } => vec![Effect::Attach {
            dir: dir.clone(),
            job: job.clone(),
        }],
        OpenTarget::Unreachable(msg) => {
            app.flash(msg.clone(), now);
            vec![]
        }
    }
}

fn step(app: &mut App, lines: &[Line], forward: bool) {
    let sel = |i: &usize| lines[*i].target().is_some();
    let next = match (selected(lines, app), forward) {
        (None, _) => (0..lines.len()).find(sel),
        (Some(i), true) => (i + 1..lines.len()).find(sel),
        (Some(i), false) => (0..i).rev().find(sel),
    };
    if let Some(i) = next {
        app.place(lines, i);
    }
}

fn selected(lines: &[Line], app: &App) -> Option<usize> {
    if let Some(t) = &app.cursor
        && let Some(i) = lines.iter().position(|l| l.target().as_ref() == Some(t))
    {
        return Some(i);
    }
    let sel = |i: &usize| lines[*i].target().is_some();
    let from = app.last_index.min(lines.len());
    (from..lines.len())
        .find(sel)
        .or_else(|| (0..from).rev().find(sel))
}

fn edit(input: &mut String, key: Key) {
    match key {
        Key::Char(c) => input.push(c),
        Key::Space => input.push(' '),
        Key::Backspace => {
            input.pop();
        }
        _ => {}
    }
}

fn non_empty(input: &str) -> Option<String> {
    let t = input.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn candidates(input: &str, options: &[GroupName]) -> Vec<GroupName> {
    let typed = input.trim();
    let needle = typed.to_lowercase();
    let mut out = Vec::new();
    if !typed.is_empty() && !options.iter().any(|g| g.0.eq_ignore_ascii_case(typed)) {
        out.push(GroupName(typed.to_owned()));
    }
    out.extend(
        options
            .iter()
            .filter(|g| g.0.to_lowercase().contains(&needle))
            .cloned(),
    );
    out
}

fn row_dir(key: &RowKey) -> &ConfigDir {
    match key {
        RowKey::Live { dir, .. } | RowKey::Exited { dir, .. } => dir,
    }
}

fn user_group<'a>(user: &'a UserState, row: &Row) -> Option<&'a GroupName> {
    user.entry(&row.user_key).and_then(|e| e.group.as_ref())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    Header {
        group: GroupId,
        title: String,
        count: usize,
        needs_you: usize,
    },
    Row {
        key: RowKey,
        status: Status,
        label: Option<String>,
        title: SafeText,
        expanded: bool,
    },
    Node {
        row: RowKey,
        id: TaskId,
        kind: NodeKind,
        state: NodeState,
        label: SafeText,
        prefix: String,
    },
    Finished {
        row: RowKey,
        count: u16,
        prefix: String,
    },
}

impl Line {
    pub fn target(&self) -> Option<Target> {
        match self {
            Line::Row { key, .. } => Some(Target::Row(key.clone())),
            Line::Node { row, id, .. } => Some(Target::Node {
                row: row.clone(),
                id: id.clone(),
            }),
            Line::Header { .. } | Line::Finished { .. } => None,
        }
    }
}

pub fn layout(snap: &Snapshot, user: &UserState, app: &App) -> Vec<Line> {
    let needle = app.filter_text().to_lowercase();
    let mut groups: BTreeMap<GroupId, Vec<&Row>> = BTreeMap::new();
    for row in snap.rows.iter().filter(|r| app.in_scope(r)) {
        groups.entry(group_of(row, user)).or_default().push(row);
    }
    let mut out = Vec::new();
    for (group, mut rows) in groups {
        let title = group_title(&group);
        let label = |r: &Row| user.entry(&r.user_key).and_then(|e| e.label.clone());
        rows.retain(|r| needle.is_empty() || matches(r, label(r).as_deref(), &title, &needle));
        if rows.is_empty() {
            continue;
        }
        rows.sort_by(|a, b| row_order(a, b).then_with(|| a.key.cmp(&b.key)));
        let needs_you = rows
            .iter()
            .filter(|r| r.status.rank() == Rank::NeedsYou)
            .count();
        out.push(Line::Header {
            group,
            title,
            count: rows.len(),
            needs_you,
        });
        for row in rows {
            let expanded = !row.tree.is_empty() && app.expanded.contains(&row.key);
            out.push(Line::Row {
                key: row.key.clone(),
                status: row.status.clone(),
                label: label(row),
                title: row.title.clone(),
                expanded,
            });
            if expanded {
                flatten(&mut out, &row.key, &row.tree, "");
            }
        }
    }
    out
}

fn group_of(row: &Row, user: &UserState) -> GroupId {
    let auto = GroupId::Auto(row.auto_group.clone());
    if row.auto_group == AutoGroup::Finished {
        return auto;
    }
    user_group(user, row)
        .and_then(|name| {
            Some(GroupId::User {
                order: user.group_order(name)?,
                name: name.clone(),
            })
        })
        .unwrap_or(auto)
}

fn group_title(group: &GroupId) -> String {
    match group {
        GroupId::User { name, .. } => name.0.clone(),
        GroupId::Auto(AutoGroup::Team(t)) => format!("team {}", t.as_str()),
        GroupId::Auto(AutoGroup::Repo(p)) => p.file_name().map_or_else(
            || p.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
        GroupId::Auto(AutoGroup::Elsewhere) => "elsewhere".into(),
        GroupId::Auto(AutoGroup::Finished) => "finished · 24h".into(),
    }
}

fn matches(row: &Row, label: Option<&str>, group: &str, needle: &str) -> bool {
    let cwd = row.detail.cwd.to_string_lossy();
    [
        Some(row.title.as_str()),
        label,
        Some(row.now.as_str()),
        row.meta.branch.as_ref().map(SafeText::as_str),
    ]
    .into_iter()
    .flatten()
    .chain([cwd.as_ref(), group])
    .any(|hay| hay.to_lowercase().contains(needle))
}

fn flatten(out: &mut Vec<Line>, row: &RowKey, tree: &Tree, indent: &str) {
    let items = tree.live.len() + usize::from(tree.finished > 0);
    for (i, node) in tree.live.iter().enumerate() {
        let last = i + 1 == items;
        out.push(Line::Node {
            row: row.clone(),
            id: node.id.clone(),
            kind: node.kind.clone(),
            state: node.state,
            label: node.label.clone(),
            prefix: format!("{indent}{}", if last { "└─" } else { "├─" }),
        });
        flatten(
            out,
            row,
            &node.children,
            &format!("{indent}{}", if last { "   " } else { "│  " }),
        );
    }
    if tree.finished > 0 {
        out.push(Line::Finished {
            row: row.clone(),
            count: tree.finished,
            prefix: format!("{indent}└─"),
        });
    }
}

pub fn render(frame: &mut Frame, app: &App, snap: &Snapshot, user: &UserState, now: At) {
    let lines = layout(snap, user, app);
    let sel = selected(&lines, app);
    let [top, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(Paragraph::new(header(app, snap, now, top.width)), top);

    if let Some(view) = &app.skills {
        skills_view::render(frame, body, app, view, user);
    } else {
        let current = sel
            .and_then(|i| lines[i].target())
            .and_then(|t| snap.rows.iter().find(|r| &r.key == t.row()));
        let mut list_area = body;
        if app.detail
            && let Some(row) = current
        {
            let wide = body.width >= 100;
            let [list, pane] = if wide {
                let list = (body.width / 2).min(LIST_WIDTH);
                Layout::horizontal([Constraint::Length(list), Constraint::Min(0)]).areas(body)
            } else {
                Layout::vertical([Constraint::Min(0), Constraint::Percentage(60)]).areas(body)
            };
            list_area = list;
            let label = user.entry(&row.user_key).and_then(|e| e.label.as_deref());
            render_detail(
                frame,
                pane,
                row,
                label,
                if wide { Borders::LEFT } else { Borders::TOP },
                now,
            );
        }
        render_list(frame, list_area, &lines, sel);
    }
    frame.render_widget(Paragraph::new(footer(app, now)), foot);

    match &app.mode {
        Mode::Help => render_help(frame, body, app, snap),
        Mode::Move {
            input,
            choice,
            options,
            ..
        } => {
            render_picker(frame, body, input, *choice, options);
        }
        _ => {}
    }
}

pub fn dump(app: &App, snap: &Snapshot, user: &UserState, now: At, width: u16) -> String {
    let body: usize = match &app.skills {
        Some(view) => skills_view::lines(app, view, user, usize::from(width))
            .0
            .len(),
        None => layout(snap, user, app).len() + 1,
    };
    let height = u16::try_from(body + 2).unwrap_or(u16::MAX);
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("TestBackend is infallible");
    term.draw(|f| render(f, app, snap, user, now))
        .expect("TestBackend is infallible");
    let mut out = buffer_text(term.backend().buffer());
    for d in &snap.diagnostics {
        out.push_str(&format!(
            "⚠ {:?}  {}  {}\n",
            d.source,
            problem_text(&d.problem),
            d.path
        ));
    }
    out
}

fn buffer_text(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..buf.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let s = buf[(x, y)].symbol();
            line.push_str(s);
            skip = s.width().saturating_sub(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// How a status looks everywhere it shows: list, details and the help legend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    NeedsYou,
    YourTurn,
    Working,
    Waiting,
    Idle,
    Ended,
}

impl Look {
    const ALL: [Look; 6] = [
        Look::NeedsYou,
        Look::YourTurn,
        Look::Working,
        Look::Waiting,
        Look::Idle,
        Look::Ended,
    ];

    fn of(status: &Status) -> Look {
        match status {
            Status::NeedsYou(_) => Look::NeedsYou,
            Status::Working => Look::Working,
            Status::Parked(_) => Look::Waiting,
            Status::Idle {
                asked_question: true,
            } => Look::YourTurn,
            Status::Idle {
                asked_question: false,
            } => Look::Idle,
            Status::Exited(_) => Look::Ended,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Look::NeedsYou => "▲",
            Look::YourTurn => "?",
            Look::Working => "●",
            Look::Waiting => "◐",
            Look::Idle => "○",
            Look::Ended => "·",
        }
    }

    fn word(self) -> &'static str {
        match self {
            Look::NeedsYou => "needs you",
            Look::YourTurn => "your turn",
            Look::Working => "working",
            Look::Waiting => "waiting",
            Look::Idle => "idle",
            Look::Ended => "ended",
        }
    }

    fn meaning(self) -> &'static str {
        match self {
            Look::NeedsYou => "blocked on a prompt or question",
            Look::YourTurn => "replied with a question",
            Look::Working => "running a turn",
            Look::Waiting => "turn over, background work out",
            Look::Idle => "nothing in flight",
            Look::Ended => "bg job ended in the last day",
        }
    }

    fn style(self) -> Style {
        match self {
            Look::NeedsYou => Style::new().fg(Color::Red),
            Look::YourTurn => Style::new().fg(Color::Yellow),
            Look::Working => Style::new().fg(Color::Green),
            Look::Waiting => Style::new().fg(Color::Cyan),
            Look::Idle => Style::new(),
            Look::Ended => dim(),
        }
    }

    /// Glyph and word, padded to the status column.
    fn label(self) -> Span<'static> {
        let text = format!("{} {}", self.glyph(), self.word());
        Span::styled(format!("{text:<STATUS_WIDTH$}"), self.style())
    }
}

fn header(app: &App, snap: &Snapshot, now: At, width: u16) -> TLine<'static> {
    let rows: Vec<&Row> = snap.rows.iter().filter(|r| app.in_scope(r)).collect();
    let live = rows
        .iter()
        .filter(|r| matches!(r.key, RowKey::Live { .. }))
        .count();
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut left = vec![Span::styled(" agentty", bold)];
    match app.workspace() {
        Some(w) => left.push(Span::styled(format!(" · {}", w.name), bold)),
        None if !app.scope.config.workspaces.is_empty() => left.push(Span::styled(" · all", dim())),
        None => {}
    }
    if app.skills.is_some() {
        left.push(Span::styled(" · skills", bold));
    }
    left.push(Span::raw(format!("  {live} live")));
    for look in [Look::NeedsYou, Look::YourTurn, Look::Waiting] {
        let n = rows.iter().filter(|r| Look::of(&r.status) == look).count();
        if n > 0 {
            left.push(Span::raw(" · "));
            left.push(Span::styled(format!("{n} {}", look.word()), look.style()));
        }
    }
    let mut right = limits_spans(app, snap, now);
    if !snap.diagnostics.is_empty() {
        right.push(Span::styled(
            format!("⚠ {}", snap.diagnostics.len()),
            Style::new().fg(Color::Yellow),
        ));
        right.push(Span::raw("  "));
    }
    right.push(Span::styled("? help ", dim()));
    spread(left, right, width)
}

/// The workspace's accounts (every configured account under "all"), each with its 5-hour and
/// 7-day use. The account `n` would pick is bold when there is a choice.
fn limits_spans(app: &App, snap: &Snapshot, now: At) -> Vec<Span<'static>> {
    let config = &app.scope.config;
    let (accounts, pick) = match app.workspace() {
        Some(w) => (
            w.accounts.clone(),
            (w.accounts.len() > 1).then(|| roomiest(&w.accounts, &snap.limits, now)),
        ),
        None if config.accounts.is_empty() => (snap.limits.keys().cloned().collect(), None),
        None => (
            config.accounts.iter().map(|a| a.dir.clone()).collect(),
            None,
        ),
    };
    let mut out = Vec::new();
    for (i, dir) in accounts.iter().enumerate() {
        let name = config.account_name(dir);
        let name_style = if pick == Some(i) {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        out.push(Span::styled(name, name_style));
        let Some(u) = snap.limits.get(dir) else {
            out.push(Span::styled(" —", dim()));
            out.push(Span::raw("  "));
            continue;
        };
        let mut live = false;
        for (label, w) in [("5h", u.five_hour), ("7d", u.seven_day)] {
            let Some(w) = w else { continue };
            out.push(Span::styled(format!(" {label} "), dim()));
            out.push(limits_pct(w, now));
            live |= w.resets_at > now;
        }
        if live && now.millis_since(u.at) > STALE_LIMITS_MS {
            out.push(Span::styled(format!(" ({})", elapsed(u.at, now)), dim()));
        }
        out.push(Span::raw("  "));
    }
    out
}

const STALE_LIMITS_MS: i64 = 60 * 60 * 1000;

fn limits_pct(w: Window, now: At) -> Span<'static> {
    if w.resets_at <= now {
        return Span::styled("fresh", Style::new().fg(Color::Green));
    }
    match w.used_pct {
        p if p >= 100.0 => Span::styled("full", Style::new().fg(Color::Red)),
        p if p >= 90.0 => Span::styled(format!("{p:.0}%"), Style::new().fg(Color::Red)),
        p if p >= 70.0 => Span::styled(format!("{p:.0}%"), Style::new().fg(Color::Yellow)),
        p => Span::raw(format!("{p:.0}%")),
    }
}

fn footer(app: &App, now: At) -> TLine<'static> {
    let prompt = |label: &str, input: &str, hint: &str| {
        TLine::from(vec![
            Span::raw(format!(" {label}{input}")),
            Span::styled("▏", dim()),
            Span::styled(format!("   {hint}"), dim()),
        ])
    };
    match &app.mode {
        Mode::Normal => match &app.flash {
            Some(f) if f.until > now => TLine::from(Span::styled(
                format!(" {}", f.text),
                Style::new().fg(Color::Yellow),
            )),
            _ if app.skills.is_some() => keys(skills_view::KEYS),
            _ if app.scope.config.workspaces.is_empty() => keys(&[
                ("⏎", "open"),
                ("tab", "details"),
                ("l", "agents"),
                ("n", "new"),
                ("r", "label"),
                ("m", "move"),
                ("s", "skills"),
                ("/", "filter"),
                ("?", "help"),
            ]),
            _ => keys(&[
                ("⏎", "open"),
                ("tab", "details"),
                ("l", "agents"),
                ("n", "new"),
                ("r", "label"),
                ("m", "move"),
                ("w", "workspace"),
                ("s", "skills"),
                ("/", "filter"),
                ("?", "help"),
            ]),
        },
        Mode::Filter { input } => prompt("/", input, "⏎ keep  esc clear"),
        Mode::Label { input, .. } => prompt("label: ", input, "⏎ save (empty clears)  esc cancel"),
        Mode::Move { input, .. } => prompt(
            "group: ",
            input,
            "↑/↓ choose  ⏎ move  - automatic  esc cancel",
        ),
        Mode::NewSession {
            accounts,
            choice,
            input,
            prompt: first,
            ..
        } => prompt(
            &format!(
                "{} on {} · name (optional): ",
                if first.is_some() { "agent" } else { "new" },
                app.scope.config.account_name(&accounts[*choice])
            ),
            input,
            if accounts.len() > 1 {
                "⏎ launch  tab account  esc cancel"
            } else {
                "⏎ launch  esc cancel"
            },
        ),
        Mode::Help => TLine::from(Span::styled(" any key closes help", dim())),
    }
}

fn render_list(frame: &mut Frame, area: Rect, lines: &[Line], sel: Option<usize>) {
    let width = usize::from(area.width);
    let mut out: Vec<TLine> = vec![TLine::from(Span::styled(
        format!(" {:<STATUS_WIDTH$}session", "status"),
        dim(),
    ))];
    let mut sel_end = 0;
    for (i, line) in lines.iter().enumerate() {
        let is_sel = sel == Some(i);
        out.push(draw_line(line, is_sel, width));
        if is_sel {
            sel_end = out.len();
        }
    }
    let scroll = sel_end.saturating_sub(usize::from(area.height));
    frame.render_widget(
        Paragraph::new(out).scroll((u16::try_from(scroll).unwrap_or(0), 0)),
        area,
    );
}

fn draw_line(line: &Line, selected: bool, width: usize) -> TLine<'static> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let line = match line {
        Line::Header {
            group,
            title,
            count,
            needs_you,
        } => {
            let style = if *group == GroupId::Auto(AutoGroup::Finished) {
                dim()
            } else {
                bold
            };
            let mut right = Vec::new();
            if *needs_you > 0 {
                right.push(Span::styled(
                    format!("▲{needs_you} "),
                    Look::NeedsYou.style(),
                ));
            }
            right.push(Span::styled(format!("{count} "), dim()));
            let used = 3 + title.width() + spans_width(&right);
            let rule = "─".repeat(width.saturating_sub(used));
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(title.clone(), style),
                Span::raw(" "),
                Span::styled(rule, dim()),
                Span::raw(" "),
            ];
            spans.extend(right);
            TLine::from(spans)
        }
        Line::Row {
            status,
            label,
            title,
            ..
        } => {
            let look = Look::of(status);
            let name = match (selected, look) {
                (true, _) => bold,
                (false, Look::Ended) => dim(),
                (false, _) => Style::new(),
            };
            let mut spans = vec![Span::raw(" "), look.label()];
            let room = width.saturating_sub(spans_width(&spans) + 1);
            match label {
                Some(l) => {
                    let l = truncate(l, room);
                    let rest = room.saturating_sub(l.width() + 2);
                    spans.push(Span::styled(l, name.add_modifier(Modifier::BOLD)));
                    if rest > 1 {
                        spans.push(Span::styled(
                            format!("  {}", truncate(title.as_str(), rest)),
                            dim(),
                        ));
                    }
                }
                None => spans.push(Span::styled(truncate(title.as_str(), room), name)),
            }
            TLine::from(spans)
        }
        Line::Node {
            kind,
            state,
            label,
            prefix,
            ..
        } => {
            let (g, style) = node_glyph(kind, *state);
            let mut spans = vec![
                Span::styled(format!("   {prefix} "), dim()),
                Span::styled(g, style),
                Span::raw(" "),
            ];
            let kind = kind_name(kind).to_owned();
            spans.push(Span::styled(format!("{kind}  "), dim()));
            let room = width.saturating_sub(spans_width(&spans) + 1);
            spans.push(Span::raw(truncate(label.as_str(), room)));
            TLine::from(spans)
        }
        Line::Finished { count, prefix, .. } => TLine::from(Span::styled(
            format!("   {prefix} ✓ {count} finished"),
            dim(),
        )),
    };
    if selected { band(line, width) } else { line }
}

/// Paints the selection band behind a whole line.
fn band(line: TLine<'static>, width: usize) -> TLine<'static> {
    let mut spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content, s.style.bg(BAND)))
        .collect();
    let pad = width.saturating_sub(spans_width(&spans));
    spans.push(Span::styled(" ".repeat(pad), Style::new().bg(BAND)));
    TLine::from(spans)
}

fn node_glyph(kind: &NodeKind, state: NodeState) -> (&'static str, Style) {
    match state {
        NodeState::Running => {
            let g = match kind {
                NodeKind::Agent { .. } => "●",
                NodeKind::Shell | NodeKind::Monitor => "◉",
                NodeKind::Workflow => "◆",
                NodeKind::Wakeup => "◷",
            };
            (g, Style::new().fg(Color::Green))
        }
        NodeState::Finished(_) => ("✓", dim()),
        NodeState::Killed => ("✗", Style::new().fg(Color::Red)),
        NodeState::Orphaned => ("⊘", dim()),
    }
}

fn kind_name(kind: &NodeKind) -> &str {
    match kind {
        NodeKind::Agent { agent_type, .. } => agent_type.as_str(),
        NodeKind::Shell => "shell",
        NodeKind::Monitor => "monitor",
        NodeKind::Workflow => "workflow",
        NodeKind::Wakeup => "wakeup",
    }
}

/// Where the session works: branch or worktree, then its PRs.
fn place(meta: &Meta) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = meta.branch.as_ref().or(meta.worktree.as_ref()) {
        parts.push(b.as_str().to_owned());
    }
    parts.extend(meta.prs.iter().map(|pr| format!("#{pr}")));
    parts.join("  ")
}

/// A bar `cells` wide filled to `pct`. Only a nearly full context gets colour.
fn gauge(pct: u8, cells: usize) -> Vec<Span<'static>> {
    let filled = (usize::from(pct) * cells).div_ceil(100).min(cells);
    let used = match pct {
        p if p >= 90 => Style::new().fg(Color::Red),
        p if p >= 70 => Style::new().fg(Color::Yellow),
        _ => Style::new(),
    };
    vec![
        Span::styled("━".repeat(filled), used),
        Span::styled("─".repeat(cells - filled), dim()),
    ]
}

fn context(ctx: Ctx) -> Vec<Span<'static>> {
    match ctx {
        Ctx::Percent(p) => {
            let mut out = gauge(p, 12);
            out.push(Span::raw(format!(" {p}%")));
            out
        }
        Ctx::Tokens(t) if t >= 1_000_000 => {
            vec![Span::raw(format!("{:.1}M tokens", t as f64 / 1e6))]
        }
        Ctx::Tokens(t) if t >= 1_000 => vec![Span::raw(format!("{}k tokens", t / 1000))],
        Ctx::Tokens(t) => vec![Span::raw(format!("{t} tokens"))],
    }
}

fn home_relative(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(PathBuf::from)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Word-wraps `text` to `width` columns, splitting words longer than a line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.width() + 1 + word.width() > width {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        for c in word.chars() {
            if cur.width() + c.width().unwrap_or(0) > width {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The details pane: what the session is doing, the facts the list leaves out, then its
/// conversation and ids. Labels sit right-aligned in one column.
fn render_detail(
    frame: &mut Frame,
    area: Rect,
    row: &Row,
    label: Option<&str>,
    border: Borders,
    now: At,
) {
    const KEY: usize = 8;
    let block = Block::new()
        .borders(border)
        .border_style(dim())
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width);
    let text_w = width.saturating_sub(KEY + 1);
    let d = &row.detail;
    let look = Look::of(&row.status);
    let bold = Style::new().add_modifier(Modifier::BOLD);

    let mut out = vec![spread(
        vec![Span::styled(
            truncate(
                label.unwrap_or(row.title.as_str()),
                width.saturating_sub(14),
            ),
            bold,
        )],
        vec![Span::styled(
            format!("{} {}", look.glyph(), look.word()),
            look.style(),
        )],
        inner.width,
    )];
    if label.is_some() {
        out.push(TLine::from(Span::styled(
            row.title.as_str().to_owned(),
            dim(),
        )));
    }
    out.push(TLine::raw(""));
    let fact = |out: &mut Vec<TLine<'static>>, key: &str, value: Vec<Span<'static>>| {
        let mut spans = vec![Span::styled(format!("{key:>KEY$} "), dim())];
        spans.extend(value);
        out.push(TLine::from(spans));
    };
    let para = |out: &mut Vec<TLine<'static>>, key: &str, text: &str, style: Style| {
        for (i, l) in wrap(text, text_w).into_iter().enumerate() {
            let key = if i == 0 { key } else { "" };
            out.push(TLine::from(vec![
                Span::styled(format!("{key:>KEY$} "), dim()),
                Span::styled(l, style),
            ]));
        }
    };

    para(&mut out, "now", row.now.as_str(), Style::new());
    let meta = &row.meta;
    let place = place(meta);
    if !place.is_empty() {
        fact(&mut out, "branch", vec![Span::raw(place)]);
    }
    if let Some(ctx) = meta.ctx {
        fact(&mut out, "context", context(ctx));
    }
    if let Some(Cost::Live(c) | Cost::Final(c)) = meta.cost {
        fact(&mut out, "cost", vec![Span::raw(format!("${c:.2}"))]);
    }
    fact(
        &mut out,
        "age",
        vec![Span::raw(format!("{} ago", elapsed(row.since, now)))],
    );
    let mut runs = match &d.job_id {
        Some(j) => format!("bg job {}", j.0),
        None if meta.bg => "bg job".to_owned(),
        None => "tmux pane".to_owned(),
    };
    if !meta.dir_tag.is_empty() {
        runs = format!("{runs} on {}", meta.dir_tag);
    }
    if meta.remote_control {
        runs.push_str(", remote control on");
    }
    fact(&mut out, "runs as", vec![Span::raw(runs)]);

    let tree = &row.tree;
    if !tree.is_empty() {
        out.push(TLine::raw(""));
    }
    for (i, node) in tree.live.iter().enumerate() {
        let (g, style) = node_glyph(&node.kind, node.state);
        let key = if i == 0 { "agents" } else { "" };
        fact(
            &mut out,
            key,
            vec![
                Span::styled(format!("{g} "), style),
                Span::styled(format!("{}  ", kind_name(&node.kind)), dim()),
                Span::raw(truncate(node.label.as_str(), text_w.saturating_sub(16))),
                Span::styled(format!("  {}", elapsed(node.since, now)), dim()),
            ],
        );
        if let Some(n) = node.now.as_ref().filter(|n| !n.is_empty()) {
            para(&mut out, "", &format!("  {}", n.as_str()), dim());
        }
    }
    if tree.finished > 0 {
        let key = if tree.live.is_empty() { "agents" } else { "" };
        fact(
            &mut out,
            key,
            vec![Span::styled(format!("✓ {} finished", tree.finished), dim())],
        );
    }

    if let Some(p) = &d.last_prompt {
        out.push(TLine::raw(""));
        para(
            &mut out,
            "you",
            p.as_str(),
            Style::new().add_modifier(Modifier::ITALIC),
        );
    }
    if let Some(t) = &d.last_text {
        out.push(TLine::raw(""));
        para(&mut out, "claude", t.as_str(), Style::new());
    }
    for (name, items) in [("tasks", &d.open_tasks), ("pending", &d.pending)] {
        if !items.is_empty() {
            out.push(TLine::raw(""));
        }
        for (i, item) in items.iter().enumerate() {
            let key = if i == 0 { name } else { "" };
            para(&mut out, key, &format!("· {}", item.as_str()), Style::new());
        }
    }

    out.push(TLine::raw(""));
    for (name, path) in [("folder", &d.cwd), ("config", &d.config_dir)] {
        if !path.as_os_str().is_empty() {
            para(&mut out, name, &home_relative(path), dim());
        }
    }
    if let Some(s) = &d.session_id {
        fact(&mut out, "session", vec![Span::styled(s.0.clone(), dim())]);
    }
    frame.render_widget(Paragraph::new(out), inner);
}

/// Keys as bold names with dim descriptions, for footers.
fn keys(pairs: &[(&str, &str)]) -> TLine<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in pairs {
        spans.push(Span::styled(
            key.to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {what}   "), dim()));
    }
    TLine::from(spans)
}

/// Keys grouped by what they're for, a legend of the status marks, what the usage numbers
/// mean, and any problems reading Claude Code's files.
fn render_help(frame: &mut Frame, body: Rect, app: &App, snap: &Snapshot) {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let column = |title: &str, rows: &[(&str, &str)]| -> Vec<TLine<'static>> {
        let mut out = vec![
            TLine::from(Span::styled(title.to_owned(), bold)),
            TLine::raw(""),
        ];
        for (key, what) in rows {
            out.push(TLine::from(vec![
                Span::styled(format!("{key:>5}  "), Style::new().fg(Color::Cyan)),
                Span::raw(what.to_string()),
            ]));
        }
        out
    };
    let mut around = vec![
        ("j k", "up and down"),
        ("g G", "top and bottom"),
        ("/", "filter by any text"),
    ];
    if !app.scope.config.workspaces.is_empty() {
        around.push(("w", "next workspace, then all"));
    }
    around.extend([
        ("s", "skills across accounts"),
        ("?", "this help"),
        ("q", "quit"),
    ]);
    let mut legend = vec![
        TLine::from(Span::styled("What the marks mean", bold)),
        TLine::raw(""),
    ];
    for look in Look::ALL {
        legend.push(TLine::from(vec![
            look.label(),
            Span::styled(look.meaning(), dim()),
        ]));
    }
    let columns = [
        column("Get around", &around),
        column(
            "Act on a session",
            &[
                ("⏎", "open it (^Z comes back)"),
                ("tab", "details"),
                ("l h", "show or hide its agents"),
                ("n", "start a new one here"),
                ("r", "label it"),
                ("m", "move to a group"),
            ],
        ),
        legend,
    ];

    let mut notes = vec![
        TLine::from(vec![
            Span::styled("Account use  ", bold),
            Span::styled(
                "5h and 7d: each account's 5-hour and 7-day limit. fresh: reset since last seen.",
                dim(),
            ),
        ]),
        TLine::from(Span::styled(
            "             n starts new sessions on the bold account, the one with the most room.",
            dim(),
        )),
        TLine::raw(""),
    ];
    if snap.diagnostics.is_empty() {
        notes.push(TLine::from(Span::styled(
            "No problems reading Claude Code's files.",
            dim(),
        )));
    } else {
        notes.push(TLine::from(Span::styled(
            match snap.diagnostics.len() {
                1 => "⚠ 1 problem reading Claude Code's files".to_owned(),
                n => format!("⚠ {n} problems reading Claude Code's files"),
            },
            Style::new().fg(Color::Yellow),
        )));
        for d in &snap.diagnostics {
            notes.push(TLine::from(Span::styled(
                format!("{:?}  {}  {}", d.source, problem_text(&d.problem), d.path),
                dim(),
            )));
        }
    }

    // As many columns across as fit, each band as tall as its tallest column.
    const COLUMN_X: [u16; 3] = [0, 32, 68];
    let width = body.width.min(112);
    let inner_w = width.saturating_sub(6);
    let across = match inner_w {
        104.. => 3,
        64.. => 2,
        _ => 1,
    };
    let bands: Vec<&[Vec<TLine>]> = columns.chunks(across).collect();
    let band_h: Vec<usize> = bands
        .iter()
        .map(|b| b.iter().map(Vec::len).max().unwrap_or(0) + 1)
        .collect();
    let cols_h: usize = band_h.iter().sum();
    let height = u16::try_from(cols_h + notes.len() + 3).unwrap_or(u16::MAX);
    let area = centered(body, width, height);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_style(dim())
        .title(Span::styled(" keys ", bold))
        .padding(Padding::new(2, 2, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut y = inner.y;
    for (band, h) in bands.into_iter().zip(band_h) {
        let h = u16::try_from(h).unwrap_or(u16::MAX).min(inner.bottom() - y);
        for (x, col) in COLUMN_X.into_iter().zip(band) {
            let r = Rect {
                x: inner.x + x,
                y,
                width: inner.width.saturating_sub(x),
                height: h,
            };
            frame.render_widget(Paragraph::new(col.clone()), r);
        }
        y += h;
    }
    let below = Rect {
        y,
        height: inner.bottom() - y,
        ..inner
    };
    frame.render_widget(Paragraph::new(notes), below);
}

fn problem_text(p: &Problem) -> String {
    match p {
        Problem::Unreadable => "unreadable".into(),
        Problem::Unparsable => "unparsable".into(),
        Problem::MissingField(f) => format!("missing {f}"),
        Problem::UnknownValue { field, value } => {
            format!("unknown {field} {:?}", SafeText::redact(value).as_str())
        }
        Problem::Unavailable => "unavailable".into(),
        Problem::Deferred => "deferred (read budget)".into(),
    }
}

fn render_picker(frame: &mut Frame, body: Rect, input: &str, choice: usize, options: &[GroupName]) {
    let picks = candidates(input, options);
    let mut text: Vec<TLine> = picks
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let name = if options.contains(g) {
                g.0.clone()
            } else {
                format!("+ new group \"{}\"", g.0)
            };
            let style = if i == choice {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            TLine::from(Span::styled(format!(" {name} "), style))
        })
        .collect();
    if text.is_empty() {
        text.push(TLine::from(Span::styled(
            " type a name to create a group",
            dim(),
        )));
    }
    text.push(TLine::from(Span::styled(" - automatic group", dim())));
    let area = centered(body, 44, text.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text).block(
            Block::bordered()
                .title(" move to group ")
                .border_style(dim()),
        ),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn spread(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) -> TLine<'static> {
    let pad = usize::from(width).saturating_sub(spans_width(&left) + spans_width(&right));
    left.push(Span::raw(" ".repeat(pad)));
    left.extend(right);
    TLine::from(left)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

pub fn elapsed(since: At, now: At) -> String {
    let s = now.millis_since(since).max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::Account;
    use crate::model::Limits;
    use crate::model::{
        Count, Detail, DiagSource, Diagnostic, JobId, JobOutcome, Need, Node, PaneId, Parked, Pid,
        ProcStart, SessionId, TaskOutcome, WaitingFor,
    };
    use crate::skills::Cell;

    const NOW: At = At(1_000_000_000);

    fn key(pid: u32) -> RowKey {
        RowKey::Live {
            dir: ConfigDir("/h/.claude".into()),
            pid: Pid(pid),
            proc_start: ProcStart::new("t"),
        }
    }

    fn parked() -> Status {
        Status::Parked(
            Parked::new(Count::Exact(2), Count::Exact(0), Count::Exact(1), 0, None).unwrap(),
        )
    }

    fn row(title: &str, pid: u32, status: Status, since: i64, group: AutoGroup) -> Row {
        Row {
            key: key(pid),
            user_key: UserKey::Session(SessionId(format!("sid-{title}"))),
            auto_group: group,
            status,
            since: At(since),
            title: SafeText::redact(title),
            now: SafeText::redact(&format!("now {title}")),
            meta: Meta::default(),
            tree: Tree::default(),
            detail: Detail {
                cwd: format!("/w/{title}").into(),
                ..Detail::default()
            },
            open: OpenTarget::Unreachable("not in tmux".into()),
        }
    }

    fn repo(name: &str) -> AutoGroup {
        AutoGroup::Repo(Path::new("/w").join(name))
    }

    fn snap(rows: Vec<Row>) -> Snapshot {
        Snapshot {
            rows,
            ..Snapshot::default()
        }
    }

    fn headers(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| match l {
                Line::Header { title, count, .. } => Some(format!("{title}:{count}")),
                _ => None,
            })
            .collect()
    }

    fn titles(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .filter_map(|l| match l {
                Line::Row { title, .. } => Some(title.as_str()),
                _ => None,
            })
            .collect()
    }

    fn label(user: &mut UserState, row: &Row, l: &str) {
        user.apply(&Op::SetLabel {
            key: row.user_key.clone(),
            label: Some(l.into()),
        });
    }

    fn move_to(user: &mut UserState, row: &Row, g: &str) {
        user.apply(&Op::SetGroup {
            key: row.user_key.clone(),
            group: Some(GroupName(g.into())),
        });
    }

    fn press(app: &mut App, s: &Snapshot, u: &UserState, keys: &[Key]) -> Vec<Effect> {
        keys.iter()
            .flat_map(|k| update(app, s, u, Input::Key(*k), NOW))
            .collect()
    }

    fn typed(s: &str) -> Vec<Key> {
        s.chars()
            .map(|c| if c == ' ' { Key::Space } else { Key::Char(c) })
            .collect()
    }

    fn cursor_title<'a>(app: &App, lines: &'a [Line]) -> Option<&'a str> {
        match &lines[selected(lines, app)?] {
            Line::Row { title, .. } => Some(title.as_str()),
            Line::Node { label, .. } => Some(label.as_str()),
            _ => None,
        }
    }

    #[test]
    fn layout_orders_user_groups_by_persisted_order_then_auto_groups_then_finished() {
        let a = row("a", 1, Status::Working, 0, repo("beta"));
        let b = row("b", 2, Status::Working, 0, repo("alpha"));
        let c = row("c", 3, Status::Working, 0, AutoGroup::Elsewhere);
        let d = row("d", 4, Status::Working, 0, repo("alpha"));
        let mut e = row(
            "e",
            5,
            Status::Exited(JobOutcome::Done),
            0,
            AutoGroup::Finished,
        );
        e.key = RowKey::Exited {
            dir: ConfigDir("/h/.claude".into()),
            job: JobId("0a1b2c3d".into()),
        };
        let mut user = UserState::default();
        user.groups.push(GroupName("empty".into()));
        move_to(&mut user, &c, "zeta");
        move_to(&mut user, &d, "omega");
        move_to(&mut user, &e, "zeta");
        let lines = layout(&snap(vec![e, d, c, b, a]), &user, &App::new(false));
        assert_eq!(
            headers(&lines),
            ["zeta:1", "omega:1", "alpha:1", "beta:1", "finished · 24h:1"]
        );
    }

    #[test]
    fn a_user_group_takes_the_row_out_of_its_repo_group() {
        let a = row("a", 1, Status::Working, 0, repo("alpha"));
        let b = row("b", 2, Status::Working, 0, repo("alpha"));
        let mut user = UserState::default();
        move_to(&mut user, &a, "mine");
        let lines = layout(&snap(vec![a, b]), &user, &App::new(false));
        assert_eq!(headers(&lines), ["mine:1", "alpha:1"]);
        assert_eq!(titles(&lines), ["a", "b"]);
    }

    #[test]
    fn layout_sorts_by_status_precedence_then_oldest_first() {
        let g = repo("r");
        let rows = vec![
            row(
                "idle",
                1,
                Status::Idle {
                    asked_question: false,
                },
                10,
                g.clone(),
            ),
            row("parked-new", 2, parked(), 90, g.clone()),
            row(
                "asked",
                3,
                Status::Idle {
                    asked_question: true,
                },
                99,
                g.clone(),
            ),
            row("working", 4, Status::Working, 99, g.clone()),
            row("parked-old", 5, parked(), 20, g.clone()),
            row(
                "needs",
                6,
                Status::NeedsYou(Need::Waiting(WaitingFor::Input)),
                99,
                g.clone(),
            ),
        ];
        let lines = layout(&snap(rows), &UserState::default(), &App::new(false));
        assert_eq!(
            titles(&lines),
            [
                "needs",
                "working",
                "parked-old",
                "parked-new",
                "asked",
                "idle"
            ]
        );
        assert!(matches!(
            lines[0],
            Line::Header {
                needs_you: 1,
                count: 6,
                ..
            }
        ));
    }

    #[test]
    fn filter_matches_label_branch_cwd_and_group_case_insensitively() {
        let mut a = row("alpha task", 1, Status::Working, 0, repo("one"));
        a.meta.branch = Some(SafeText::ours("feat/Fees"));
        let b = row("beta task", 2, Status::Working, 0, repo("one"));
        let c = row("gamma task", 3, Status::Working, 0, repo("two"));
        let d = row("delta task", 4, Status::Working, 0, repo("three"));
        let mut user = UserState::default();
        label(&mut user, &b, "FEE review");
        let s = snap(vec![a, b, c, d]);
        let mut app = App::new(false);
        app.filter = "fee".into();
        let lines = layout(&s, &user, &app);
        assert_eq!(headers(&lines), ["one:2"]);
        assert_eq!(titles(&lines), ["alpha task", "beta task"]);
        app.filter = "/w/gamma".into();
        assert_eq!(titles(&layout(&s, &user, &app)), ["gamma task"]);
        app.filter = "THREE".into();
        assert_eq!(titles(&layout(&s, &user, &app)), ["delta task"]);
    }

    fn node(id: &str, kind: NodeKind, children: Tree) -> Node {
        Node {
            id: TaskId(id.into()),
            kind,
            state: NodeState::Running,
            label: SafeText::redact(id),
            since: At(0),
            now: None,
            children,
        }
    }

    fn agent() -> NodeKind {
        NodeKind::Agent {
            agent_type: SafeText::ours("Explore"),
            model: Some(SafeText::ours("sonnet")),
        }
    }

    #[test]
    fn dump_lists_diagnostics_with_unknown_values_redacted() {
        let mut s = snap(vec![]);
        s.diagnostics.push(Diagnostic {
            source: DiagSource::Registry,
            problem: Problem::UnknownValue {
                field: "status",
                value: "ghp_0123456789abcdefghijABCDEFGHIJ0123".into(),
            },
            path: "~/.claude/sessions/1.json".into(),
        });
        let text = dump(&App::new(false), &s, &UserState::default(), NOW, 100);
        assert!(
            text.contains("⚠ Registry  unknown status \"…\"  ~/.claude/sessions/1.json"),
            "{text}"
        );
        assert!(!text.contains("ghp_"), "{text}");
    }

    fn tree_row() -> Row {
        let child = Tree {
            live: vec![node("a1", agent(), Tree::default())],
            finished: 2,
        };
        let mut r = row("fan", 1, parked(), 0, repo("r"));
        r.tree = Tree {
            live: vec![
                node("a", agent(), child),
                node("mon", NodeKind::Monitor, Tree::default()),
            ],
            finished: 4,
        };
        r
    }

    #[test]
    fn expanded_rows_flatten_depth_first_with_rails_and_finished_counts() {
        let s = snap(vec![
            tree_row(),
            row("plain", 2, Status::Working, 0, repo("r")),
        ]);
        let mut app = App::new(false);
        let collapsed = layout(&s, &UserState::default(), &app);
        assert_eq!(collapsed.len(), 3);
        app.expanded.insert(key(1));
        app.expanded.insert(key(2));
        let lines = layout(&s, &UserState::default(), &app);
        let shape: Vec<String> = lines
            .iter()
            .map(|l| match l {
                Line::Header { .. } => "header".into(),
                Line::Row {
                    title, expanded, ..
                } => format!("row {} {expanded}", title.as_str()),
                Line::Node { prefix, id, .. } => format!("{prefix} {}", id.0),
                Line::Finished { prefix, count, .. } => format!("{prefix} ✓{count}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "header",
                "row plain false",
                "row fan true",
                "├─ a",
                "│  ├─ a1",
                "│  └─ ✓2",
                "├─ mon",
                "└─ ✓4",
            ]
        );
    }

    #[test]
    fn cursor_follows_its_row_across_a_re_sort_and_navigates_in_the_new_order() {
        let g = repo("r");
        let user = UserState::default();
        let before = snap(vec![
            row("a", 1, Status::Working, 0, g.clone()),
            row(
                "b",
                2,
                Status::Idle {
                    asked_question: false,
                },
                0,
                g.clone(),
            ),
            row(
                "c",
                3,
                Status::Idle {
                    asked_question: false,
                },
                5,
                g.clone(),
            ),
        ]);
        let mut app = App::new(false);
        press(&mut app, &before, &user, &[Key::Char('j')]);
        assert_eq!(cursor_title(&app, &layout(&before, &user, &app)), Some("b"));

        let after = snap(vec![
            row("a", 1, Status::Working, 0, g.clone()),
            row(
                "b",
                2,
                Status::NeedsYou(Need::Waiting(WaitingFor::Input)),
                0,
                g.clone(),
            ),
            row(
                "c",
                3,
                Status::Idle {
                    asked_question: false,
                },
                5,
                g.clone(),
            ),
        ]);
        assert_eq!(cursor_title(&app, &layout(&after, &user, &app)), Some("b"));
        press(&mut app, &after, &user, &[Key::Char('j')]);
        assert_eq!(cursor_title(&app, &layout(&after, &user, &app)), Some("a"));
    }

    #[test]
    fn a_vanished_cursor_row_hands_the_cursor_to_the_row_that_took_its_place() {
        let g = repo("r");
        let user = UserState::default();
        let rows = |ids: &[u32]| {
            snap(
                ids.iter()
                    .map(|&p| {
                        row(
                            &format!("r{p}"),
                            p,
                            Status::Working,
                            i64::from(p),
                            g.clone(),
                        )
                    })
                    .collect(),
            )
        };
        let mut app = App::new(false);
        let s = rows(&[1, 2, 3]);
        press(&mut app, &s, &user, &[Key::Char('j')]);
        assert_eq!(cursor_title(&app, &layout(&s, &user, &app)), Some("r2"));
        let gone = rows(&[1, 3]);
        assert_eq!(cursor_title(&app, &layout(&gone, &user, &app)), Some("r3"));
    }

    fn pane_row() -> Row {
        let mut r = row("p", 1, Status::Working, 0, repo("r"));
        r.open = OpenTarget::Pane(PaneId::parse("%7").unwrap());
        r
    }

    #[test]
    fn popup_enter_jumps_then_quits_only_after_jumped() {
        let s = snap(vec![pane_row()]);
        let user = UserState::default();
        let mut app = App::new(true);
        let fx = press(&mut app, &s, &user, &[Key::Enter]);
        assert_eq!(fx, [Effect::Jump(PaneId::parse("%7").unwrap())]);
        assert_eq!(
            update(&mut app, &s, &user, Input::Done(Outcome::Jumped), NOW),
            [Effect::Quit]
        );
    }

    #[test]
    fn a_failed_jump_flashes_and_keeps_the_popup_open() {
        let s = snap(vec![pane_row()]);
        let user = UserState::default();
        let mut app = App::new(true);
        let fx = update(
            &mut app,
            &s,
            &user,
            Input::Done(Outcome::Failed("pane gone".into())),
            NOW,
        );
        assert_eq!(fx, []);
        assert!(dump(&app, &s, &user, NOW, 100).contains("pane gone"));
    }

    #[test]
    fn the_persistent_window_stays_open_after_a_jump() {
        let s = snap(vec![pane_row()]);
        let mut app = App::new(false);
        assert_eq!(
            update(
                &mut app,
                &s,
                &UserState::default(),
                Input::Done(Outcome::Jumped),
                NOW
            ),
            []
        );
    }

    #[test]
    fn enter_on_an_unreachable_row_only_flashes() {
        let s = snap(vec![row("vscode", 1, Status::Working, 0, repo("r"))]);
        let user = UserState::default();
        let mut app = App::new(true);
        assert_eq!(press(&mut app, &s, &user, &[Key::Enter]), []);
        assert!(dump(&app, &s, &user, NOW, 100).contains("not in tmux"));
    }

    #[test]
    fn enter_on_a_bg_row_attaches() {
        let mut r = row("bg", 1, Status::Working, 0, repo("r"));
        let dir = ConfigDir("/h/.claude-personal".into());
        r.open = OpenTarget::Attach {
            dir: dir.clone(),
            job: JobId("0a1b2c3d".into()),
        };
        let s = snap(vec![r]);
        let fx = press(
            &mut App::new(false),
            &s,
            &UserState::default(),
            &[Key::Enter],
        );
        assert_eq!(
            fx,
            [Effect::Attach {
                dir,
                job: JobId("0a1b2c3d".into()),
            }]
        );
    }

    #[test]
    fn the_popup_stays_open_after_an_attach_returns() {
        let s = snap(vec![pane_row()]);
        let mut app = App::new(true);
        assert_eq!(
            update(
                &mut app,
                &s,
                &UserState::default(),
                Input::Done(Outcome::Attached),
                NOW
            ),
            []
        );
    }

    fn account(name: &str) -> Account {
        Account {
            name: name.into(),
            dir: ConfigDir(format!("/h/.claude-{name}").into()),
        }
    }

    /// blade (two accounts) and jnj (one), under /w.
    fn workspaces() -> Config {
        let (personal, wukong, jnj) = (account("personal"), account("wukong"), account("jnj"));
        Config {
            workspaces: vec![
                Workspace {
                    name: "blade".into(),
                    root: "/w/blade".into(),
                    accounts: vec![personal.dir.clone(), wukong.dir.clone()],
                },
                Workspace {
                    name: "jnj".into(),
                    root: "/w/jnj".into(),
                    accounts: vec![jnj.dir.clone()],
                },
            ],
            accounts: vec![personal, wukong, jnj],
            shared_skills: Some("/h/.claude-personal/skills".into()),
        }
    }

    fn limits(seven_day: f64, resets_at: At) -> Limits {
        Limits {
            five_hour: None,
            seven_day: Some(Window {
                used_pct: seven_day,
                resets_at,
            }),
            at: At(NOW.0 - 1000),
        }
    }

    #[test]
    fn a_workspace_shows_only_rows_under_its_root_and_w_cycles_to_all() {
        let s = snap(vec![
            row("blade/api", 1, Status::Working, 0, repo("api")),
            row("jnj/web", 2, Status::Working, 0, repo("web")),
            row("elsewhere", 3, Status::Working, 0, repo("x")),
        ]);
        let user = UserState::default();
        let mut app = App::new(false).scoped(workspaces(), Some(0), "/w/blade".into());
        let shown = |app: &App| -> Vec<String> {
            titles(&layout(&s, &user, app))
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(shown(&app), ["blade/api"]);
        assert!(dump(&app, &s, &user, NOW, 120).starts_with(" agentty · blade  1 live"));
        press(&mut app, &s, &user, &[Key::Char('w')]);
        assert_eq!(shown(&app), ["jnj/web"]);
        press(&mut app, &s, &user, &[Key::Char('w')]);
        assert_eq!(shown(&app).len(), 3);
        assert!(dump(&app, &s, &user, NOW, 120).starts_with(" agentty · all  3 live"));
        press(&mut app, &s, &user, &[Key::Char('w')]);
        assert_eq!(shown(&app), ["blade/api"]);
    }

    #[test]
    fn w_without_workspaces_says_where_to_add_them() {
        let s = snap(vec![row("a", 1, Status::Working, 0, repo("a"))]);
        let mut app = App::new(false);
        press(&mut app, &s, &UserState::default(), &[Key::Char('w')]);
        assert!(dump(&app, &s, &UserState::default(), NOW, 120).contains("config.toml"));
    }

    #[test]
    fn n_in_a_workspace_starts_on_the_account_with_most_room_and_tab_switches() {
        let config = workspaces();
        let (personal, wukong) = (
            config.accounts[0].dir.clone(),
            config.accounts[1].dir.clone(),
        );
        let mut s = snap(vec![row("jnj/web", 1, Status::Working, 0, repo("web"))]);
        // wukong was 97% full, but its 7-day window has reset since; personal is half used.
        s.limits = BTreeMap::from([
            (personal.clone(), limits(52.0, At(NOW.0 + 3_600_000))),
            (wukong.clone(), limits(97.0, At(NOW.0 - 60_000))),
        ]);
        let user = UserState::default();
        // No blade row is on screen, and agentty started outside blade: start at the root.
        let mut app = App::new(false).scoped(config, Some(0), "/elsewhere".into());
        let expect = |dir: &ConfigDir| Effect::NewSession {
            dir: dir.clone(),
            cwd: "/w/blade".into(),
            group: None,
            name: None,
            prompt: None,
        };
        press(&mut app, &s, &user, &[Key::Char('n')]);
        assert!(dump(&app, &s, &user, NOW, 120).contains("new on wukong · name (optional)"));
        assert_eq!(press(&mut app, &s, &user, &[Key::Enter]), [expect(&wukong)]);
        assert_eq!(
            press(&mut app, &s, &user, &[Key::Char('n'), Key::Tab, Key::Enter]),
            [expect(&personal)]
        );
    }

    #[test]
    fn the_header_shows_each_workspace_account_and_bolds_n_s_pick() {
        let config = workspaces();
        let (personal, wukong) = (
            config.accounts[0].dir.clone(),
            config.accounts[1].dir.clone(),
        );
        let mut s = snap(vec![]);
        s.limits = BTreeMap::from([
            (personal, limits(52.0, At(NOW.0 + 3_600_000))),
            (wukong, limits(97.0, At(NOW.0 - 60_000))),
        ]);
        let app = App::new(false).scoped(config, Some(0), "/w/blade".into());
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(120, 3)).unwrap();
        term.draw(|f| render(f, &app, &s, &UserState::default(), NOW))
            .unwrap();
        let top = buffer_text(term.backend().buffer());
        let top = top.lines().next().unwrap();
        assert!(top.contains("personal 7d 52%  wukong 7d fresh"), "{top}");
        let buf = term.backend().buffer();
        let x = top.find("wukong").unwrap();
        let x = u16::try_from(top[..x].chars().count()).unwrap();
        assert!(buf[(x, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn new_session_prompts_for_an_optional_name_and_files_the_spawn_into_the_group() {
        let r = row("src", 1, Status::Working, 0, repo("r"));
        let mut user = UserState::default();
        move_to(&mut user, &r, "side");
        let s = snap(vec![r]);
        let mut app = App::new(false);
        let expect = |name: Option<&str>| Effect::NewSession {
            dir: ConfigDir("/h/.claude".into()),
            cwd: "/w/src".into(),
            group: Some(GroupName("side".into())),
            name: name.map(str::to_owned),
            prompt: None,
        };
        assert_eq!(
            press(&mut app, &s, &user, &[Key::Char('n'), Key::Enter]),
            [expect(None)]
        );
        let mut keys = vec![Key::Char('n')];
        keys.extend(typed("fix the  bug "));
        keys.push(Key::Enter);
        assert_eq!(
            press(&mut app, &s, &user, &keys),
            [expect(Some("fix the  bug"))]
        );
        assert_eq!(app.mode, Mode::Normal);

        let spawned = Outcome::Spawned {
            dir: ConfigDir("/h/.claude".into()),
            job: JobId("0a1b2c3d".into()),
            group: Some(GroupName("side".into())),
        };
        assert_eq!(
            update(&mut app, &s, &user, Input::Done(spawned), NOW),
            [
                Effect::Commit(Op::SetGroup {
                    key: UserKey::Job(JobId("0a1b2c3d".into())),
                    group: Some(GroupName("side".into()))
                }),
                Effect::Attach {
                    dir: ConfigDir("/h/.claude".into()),
                    job: JobId("0a1b2c3d".into()),
                }
            ]
        );
        let mut esc = vec![Key::Char('n')];
        esc.extend(typed("x"));
        esc.push(Key::Esc);
        assert_eq!(press(&mut app, &s, &user, &esc), []);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn label_mode_edits_the_current_label_and_empty_clears_it() {
        let r = row("t", 1, Status::Working, 0, repo("r"));
        let k = r.user_key.clone();
        let mut user = UserState::default();
        label(&mut user, &r, "ab");
        let s = snap(vec![r]);
        let mut app = App::new(false);
        let fx = press(
            &mut app,
            &s,
            &user,
            &[Key::Char('r'), Key::Char('c'), Key::Enter],
        );
        assert_eq!(
            fx,
            [Effect::Commit(Op::SetLabel {
                key: k.clone(),
                label: Some("abc".into())
            })]
        );
        let fx = press(
            &mut app,
            &s,
            &user,
            &[Key::Char('r'), Key::Backspace, Key::Backspace, Key::Enter],
        );
        assert_eq!(
            fx,
            [Effect::Commit(Op::SetLabel {
                key: k.clone(),
                label: None
            })]
        );
        assert_eq!(
            press(
                &mut app,
                &s,
                &user,
                &[Key::Char('r'), Key::Char('z'), Key::Esc]
            ),
            []
        );
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn move_mode_picks_creates_and_returns_to_automatic() {
        let r = row("t", 1, Status::Working, 0, repo("r"));
        let k = r.user_key.clone();
        let user = UserState {
            groups: vec![GroupName("jnj-web".into()), GroupName("side".into())],
            ..UserState::default()
        };
        let s = snap(vec![r]);
        let mut app = App::new(false);
        let set = |g: Option<&str>| {
            vec![Effect::Commit(Op::SetGroup {
                key: k.clone(),
                group: g.map(|g| GroupName(g.into())),
            })]
        };
        assert_eq!(
            press(
                &mut app,
                &s,
                &user,
                &[Key::Char('m'), Key::Down, Key::Enter]
            ),
            set(Some("side"))
        );
        assert_eq!(
            press(&mut app, &s, &user, &[Key::Char('m'), Key::Char('-')]),
            set(None)
        );
        let mut keys = vec![Key::Char('m')];
        keys.extend(typed("jk"));
        keys.push(Key::Enter);
        assert_eq!(press(&mut app, &s, &user, &keys), set(Some("jk")));
        let mut keys = vec![Key::Char('m')];
        keys.extend(typed("SIDE"));
        keys.push(Key::Enter);
        assert_eq!(press(&mut app, &s, &user, &keys), set(Some("side")));
        let mut keys = vec![Key::Char('m')];
        keys.extend(typed("jnj"));
        keys.extend([Key::Down, Key::Enter]);
        assert_eq!(press(&mut app, &s, &user, &keys), set(Some("jnj-web")));
        assert_eq!(press(&mut app, &s, &user, &[Key::Char('m'), Key::Esc]), []);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn filter_narrows_live_enter_keeps_and_esc_clears_before_quitting() {
        let s = snap(vec![
            row("fees", 1, Status::Working, 0, repo("r")),
            row("docs", 2, Status::Working, 0, repo("r")),
        ]);
        let user = UserState::default();
        let mut app = App::new(false);
        press(
            &mut app,
            &s,
            &user,
            &[Key::Char('/'), Key::Char('f'), Key::Char('e')],
        );
        assert_eq!(titles(&layout(&s, &user, &app)), ["fees"]);
        press(&mut app, &s, &user, &[Key::Enter]);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(titles(&layout(&s, &user, &app)), ["fees"]);
        assert_eq!(press(&mut app, &s, &user, &[Key::Esc]), []);
        assert_eq!(titles(&layout(&s, &user, &app)), ["fees", "docs"]);
        assert_eq!(press(&mut app, &s, &user, &[Key::Esc]), [Effect::Quit]);

        let mut app = App::new(false);
        press(
            &mut app,
            &s,
            &user,
            &[Key::Char('/'), Key::Char('d'), Key::Esc],
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(titles(&layout(&s, &user, &app)), ["fees", "docs"]);
    }

    #[test]
    fn help_closes_on_any_key_without_quitting() {
        let s = snap(vec![row("a", 1, Status::Working, 0, repo("r"))]);
        let user = UserState::default();
        let mut app = App::new(false);
        press(&mut app, &s, &user, &[Key::Char('?')]);
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(press(&mut app, &s, &user, &[Key::Char('q')]), []);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            press(&mut app, &s, &user, &[Key::Char('q')]),
            [Effect::Quit]
        );
    }

    #[test]
    fn expand_collapse_and_detail_toggles() {
        let s = snap(vec![tree_row()]);
        let user = UserState::default();
        let mut app = App::new(false);
        press(&mut app, &s, &user, &[Key::Char('l'), Key::Char('j')]);
        assert_eq!(cursor_title(&app, &layout(&s, &user, &app)), Some("a"));
        press(&mut app, &s, &user, &[Key::Char('h')]);
        assert_eq!(cursor_title(&app, &layout(&s, &user, &app)), Some("fan"));
        press(&mut app, &s, &user, &[Key::Left]);
        assert_eq!(layout(&s, &user, &app).len(), 2);
        press(&mut app, &s, &user, &[Key::Tab]);
        assert!(app.detail);
        press(&mut app, &s, &user, &[Key::Char('p')]);
        assert!(!app.detail);
    }

    /// jot lives in personal (wukong links it, jnj lacks it), verify is jnj's own, and
    /// danger-keys is shared everywhere.
    fn inventory(config: &Config) -> Inventory {
        let dirs: Vec<ConfigDir> = config.accounts.iter().map(|a| a.dir.clone()).collect();
        let has = |p: &str, linked| Cell::Has {
            real: p.into(),
            linked,
        };
        let skill = |name: &str, cells: Vec<Cell>| crate::skills::Skill {
            name: name.into(),
            cells,
            differ: false,
        };
        let jot = "/h/.claude-personal/skills/jot";
        let keys = "/h/.claude-personal/skills/danger-keys";
        Inventory {
            folders: vec![
                Some("/h/.claude-personal/skills".into()),
                Some("/h/.claude-personal/skills".into()),
                Some("/h/.claude-jnj/skills".into()),
            ],
            accounts: dirs,
            skills: vec![
                skill(
                    "danger-keys",
                    vec![has(keys, false), has(keys, true), has(keys, true)],
                ),
                skill("jot", vec![has(jot, false), has(jot, true), Cell::Missing]),
                skill(
                    "verify",
                    vec![
                        Cell::Missing,
                        Cell::Missing,
                        has("/h/.claude-jnj/skills/verify", false),
                    ],
                ),
            ],
        }
    }

    fn skills_text(app: &App, user: &UserState) -> Vec<String> {
        dump(app, &snap(vec![]), user, NOW, 120)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn s_scans_every_account_and_the_view_puts_skills_needing_action_first() {
        let config = workspaces();
        let accounts: Vec<ConfigDir> = config.accounts.iter().map(|a| a.dir.clone()).collect();
        let inv = inventory(&config);
        let s = snap(vec![]);
        let mut user = UserState::default();
        let mut app = App::new(false).scoped(config, None, "/w".into());
        assert_eq!(
            press(&mut app, &s, &user, &[Key::Char('s')]),
            [Effect::ScanSkills(accounts.clone())]
        );
        update(&mut app, &s, &user, Input::Done(Outcome::Skills(inv)), NOW);
        let text = skills_text(&app, &user);
        assert!(text[0].starts_with(" agentty · all · skills"), "{text:?}");
        assert_eq!(
            text[1..10],
            [
                "                personal  wukong  jnj",
                "",
                " Needs sharing  2",
                " ▌ jot          ●         →       ·    Missing on jnj",
                "                ⏎ starts a share-skill agent that keeps one copy and links it into every account",
                "   verify       ·         ·       ●    Missing on personal and wukong",
                "",
                " Shared everywhere  1",
                "   danger-keys  ●         →       →",
            ],
            "{text:#?}"
        );
        assert!(
            text[11].ends_with("personal and wukong share one skills folder"),
            "{text:#?}"
        );

        let x = press(&mut app, &s, &user, &[Key::Char('j'), Key::Char('x')]);
        let local = Op::SetSkillLocal {
            name: "verify".into(),
            local: true,
        };
        assert_eq!(x, [Effect::Commit(local.clone())]);
        user.apply(&local);
        let text = skills_text(&app, &user);
        assert_eq!(
            text[9..13],
            [
                " Kept local  1",
                " ▌ verify       ·         ·       ●    Kept on these accounts only",
                "                x shares it again",
                "",
            ],
            "{text:#?}"
        );
        assert_eq!(
            press(&mut app, &s, &user, &[Key::Char('x')]),
            [Effect::Commit(Op::SetSkillLocal {
                name: "verify".into(),
                local: false,
            })]
        );

        assert_eq!(
            update(&mut app, &s, &user, Input::Done(Outcome::Attached), NOW),
            [Effect::ScanSkills(accounts)]
        );
        press(&mut app, &s, &user, &[Key::Esc]);
        assert!(app.skills.is_none());
        assert_eq!(press(&mut app, &s, &user, &[Key::Esc]), [Effect::Quit]);
    }

    #[test]
    fn enter_starts_a_share_skill_agent_with_the_prompt_on_the_roomiest_account() {
        let config = workspaces();
        let inv = inventory(&config);
        let (personal, wukong) = (
            config.accounts[0].dir.clone(),
            config.accounts[1].dir.clone(),
        );
        let mut s = snap(vec![]);
        s.limits = BTreeMap::from([
            (personal, limits(90.0, At(NOW.0 + 3_600_000))),
            (wukong.clone(), limits(10.0, At(NOW.0 + 3_600_000))),
        ]);
        let user = UserState::default();
        let mut app = App::new(false)
            .scoped(config, Some(0), "/w/blade/api".into())
            .with_skills(inv.clone());
        press(&mut app, &s, &user, &[Key::Enter]);
        let footer = dump(&app, &s, &user, NOW, 120);
        assert!(
            footer.contains("agent on wukong · name (optional): share jot"),
            "{footer}"
        );
        let effects = press(&mut app, &s, &user, &[Key::Enter]);
        let shared = Path::new("/h/.claude-personal/skills");
        let names = |d: &ConfigDir| app.scope.config.account_name(d);
        assert_eq!(
            effects,
            [Effect::NewSession {
                dir: wukong,
                cwd: "/w/blade/api".into(),
                group: None,
                name: Some("share jot".into()),
                prompt: Some(inv.share_prompt(&inv.skills[1], shared, names)),
            }]
        );
        assert!(
            app.skills.is_some(),
            "the view stays for when the agent returns"
        );
    }

    #[test]
    fn enter_on_a_shared_skill_or_without_a_shared_folder_only_flashes() {
        let mut config = workspaces();
        let inv = inventory(&config);
        let s = snap(vec![]);
        let user = UserState::default();
        let mut app = App::new(false)
            .scoped(config.clone(), None, "/w".into())
            .with_skills(inv.clone());
        assert_eq!(
            press(&mut app, &s, &user, &[Key::Char('G'), Key::Enter]),
            []
        );
        assert!(
            skills_text(&app, &user)
                .last()
                .unwrap()
                .contains("already shared")
        );

        config.shared_skills = None;
        let mut app = App::new(false)
            .scoped(config, None, "/w".into())
            .with_skills(inv);
        assert_eq!(press(&mut app, &s, &user, &[Key::Enter]), []);
        assert_eq!(app.mode, Mode::Normal);
        assert!(
            skills_text(&app, &user)
                .last()
                .unwrap()
                .contains("[skills] shared")
        );
    }

    #[test]
    fn elapsed_formats_each_unit_boundary() {
        let at = |s: i64| elapsed(At(NOW.0 - s * 1000), NOW);
        assert_eq!(
            [at(42), at(60), at(3599), at(3600), at(86_400), at(-5)],
            ["42s", "1m", "59m", "1h", "1d", "0s"]
        );
    }

    #[test]
    fn test_backend_render_smoke() {
        let mut needs = row(
            "Fix token refresh race",
            1,
            Status::NeedsYou(Need::Permission {
                tool: Some("Bash".into()),
            }),
            NOW.0 - 180_000,
            repo("jnj-web"),
        );
        needs.now = SafeText::ours("permission prompt · Bash");
        needs.meta = Meta {
            branch: Some(SafeText::ours("feat/auth-race")),
            prs: vec![4812],
            ctx: Some(Ctx::Tokens(412_000)),
            ..Meta::default()
        };
        let mut fan = tree_row();
        fan.tree.live[1].state = NodeState::Finished(TaskOutcome::Completed);
        let mut s = snap(vec![
            needs,
            fan,
            row(
                "quiet",
                3,
                Status::Idle {
                    asked_question: true,
                },
                0,
                AutoGroup::Elsewhere,
            ),
        ]);
        s.diagnostics.push(Diagnostic {
            source: DiagSource::Registry,
            problem: Problem::Unparsable,
            path: "~/.claude/sessions/1.json".into(),
        });
        let mut user = UserState::default();
        label(&mut user, &s.rows[0], "auth-race");
        let mut app = App::new(false);
        app.expanded.insert(key(1));
        let draw = |app: &App| {
            let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            term.draw(|f| render(f, app, &s, &user, NOW)).unwrap();
            term.backend().buffer().clone()
        };
        let buf = draw(&app);
        let text = buffer_text(&buf);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].starts_with(" agentty  3 live · 1 needs you · 1 your turn · 1 waiting"),
            "{text}"
        );
        assert!(lines[0].ends_with("⚠ 1  ? help"), "{text}");
        assert_eq!(lines[1], " status       session");
        assert!(
            lines[2].starts_with(" jnj-web ─") && lines[2].ends_with("▲1 1"),
            "{text}"
        );
        assert_eq!(lines[3], " ▲ needs you  auth-race  Fix token refresh race");
        assert_eq!(buf[(0, 3)].bg, BAND, "the selected row is banded");
        assert_eq!(buf[(0, 5)].bg, Color::Reset);
        assert!(!text.contains("feat/auth-race"), "{text}");
        assert!(!text.contains("permission prompt"), "{text}");
        assert!(text.contains("│  ├─ ● Explore  a1"), "{text}");
        assert!(text.contains("├─ ✓ monitor  mon"), "{text}");
        assert!(text.contains("   └─ ✓ 4 finished"), "{text}");
        assert!(text.contains(" ? your turn  quiet"), "{text}");
        assert!(lines[29].starts_with(" ⏎ open   tab details"), "{text}");

        app.detail = true;
        let text = buffer_text(&draw(&app));
        for fact in [
            "▲ needs you",
            "     now permission prompt · Bash",
            "  branch feat/auth-race  #4812",
            " context 412k tokens",
            "     age 3m ago",
        ] {
            assert!(text.contains(fact), "{fact:?} missing from\n{text}");
        }

        app.mode = Mode::Help;
        let text = buffer_text(&draw(&app));
        for part in [
            "Get around",
            "Act on a session",
            "What the marks mean",
            "? your turn  replied with a question",
            "⚠ 1 problem reading Claude Code's files",
            "Registry  unparsable  ~/.claude/sessions/1.json",
        ] {
            assert!(text.contains(part), "{part:?} missing from\n{text}");
        }
    }
}
