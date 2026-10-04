use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::model::{
    At, AutoGroup, ConfigDir, Cost, Ctx, Effect, GroupId, GroupName, Meta, NodeKind, NodeState,
    Op, OpenTarget, Outcome, Problem, Rank, Row, RowKey, SafeText, Snapshot, Status, TaskId,
    Tree, UserKey, row_order,
};
use crate::user_state::UserState;

const FLASH_MS: i64 = 4000;
const INDENT: &str = "     ";

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
    Filter { input: String },
    Label { key: UserKey, input: String },
    Move { key: UserKey, input: String, choice: usize, options: Vec<GroupName> },
    NewSession { dir: ConfigDir, cwd: PathBuf, group: Option<GroupName>, input: String },
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
        || ev.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
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
        self.flash = Some(Flash { text, until: At(now.0 + FLASH_MS) });
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
        Mode::Label { key: user_key, mut input } => match key {
            Key::Enter => vec![Effect::Commit(Op::SetLabel { key: user_key, label: non_empty(&input) })],
            Key::Esc => vec![],
            _ => {
                edit(&mut input, key);
                app.mode = Mode::Label { key: user_key, input };
                vec![]
            }
        },
        Mode::Move { key: user_key, mut input, mut choice, options } => {
            let picks = candidates(&input, &options);
            match key {
                Key::Esc => return vec![],
                Key::Char('-') if input.is_empty() => {
                    return vec![Effect::Commit(Op::SetGroup { key: user_key, group: None })];
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
            app.mode = Mode::Move { key: user_key, input, choice, options };
            vec![]
        }
        Mode::NewSession { dir, cwd, group, mut input } => match key {
            Key::Enter => vec![Effect::NewSession { dir, cwd, group, name: non_empty(&input) }],
            Key::Esc => vec![],
            _ => {
                edit(&mut input, key);
                app.mode = Mode::NewSession { dir, cwd, group, input };
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
    let row = app.cursor.as_ref().and_then(|t| snap.rows.iter().find(|r| &r.key == t.row()));
    let selectable: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].target().is_some()).collect();
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
                let input =
                    user.entry(&r.user_key).and_then(|e| e.label.clone()).unwrap_or_default();
                app.mode = Mode::Label { key: r.user_key.clone(), input };
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
            if let Some(r) = row {
                app.mode = Mode::NewSession {
                    dir: row_dir(&r.key).clone(),
                    cwd: r.detail.cwd.clone(),
                    group: user_group(user, r).cloned(),
                    input: String::new(),
                };
            }
        }
        Key::Char('/') => app.mode = Mode::Filter { input: app.filter.clone() },
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
        Outcome::Jumped | Outcome::Attached => quit.into_iter().collect(),
        Outcome::Spawned { session, group } => group
            .map(|g| {
                Effect::Commit(Op::SetGroup { key: UserKey::Session(session), group: Some(g) })
            })
            .into_iter()
            .chain(quit)
            .collect(),
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
        OpenTarget::Attach { dir, job, title } => {
            vec![Effect::Attach { dir: dir.clone(), job: job.clone(), title: title.clone() }]
        }
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
    (from..lines.len()).find(sel).or_else(|| (0..from).rev().find(sel))
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
    out.extend(options.iter().filter(|g| g.0.to_lowercase().contains(&needle)).cloned());
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
    Header { group: GroupId, title: String, count: usize, needs_you: usize },
    Row {
        key: RowKey,
        status: Status,
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

    fn height(&self) -> usize {
        match self {
            Line::Row { .. } => 2,
            _ => 1,
        }
    }
}

pub fn layout(snap: &Snapshot, user: &UserState, app: &App) -> Vec<Line> {
    let needle = app.filter_text().to_lowercase();
    let mut groups: BTreeMap<GroupId, Vec<&Row>> = BTreeMap::new();
    for row in &snap.rows {
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
        let needs_you = rows.iter().filter(|r| r.status.rank() == Rank::NeedsYou).count();
        out.push(Line::Header { group, title, count: rows.len(), needs_you });
        for row in rows {
            let expanded = !row.tree.is_empty() && app.expanded.contains(&row.key);
            out.push(Line::Row {
                key: row.key.clone(),
                status: row.status.clone(),
                label: label(row),
                title: row.title.clone(),
                meta: row.meta.clone(),
                now: row.now.clone(),
                since: row.since,
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
        .and_then(|name| Some(GroupId::User { order: user.group_order(name)?, name: name.clone() }))
        .unwrap_or(auto)
}

fn group_title(group: &GroupId) -> String {
    match group {
        GroupId::User { name, .. } => name.0.clone(),
        GroupId::Auto(AutoGroup::Team(t)) => format!("team {t}"),
        GroupId::Auto(AutoGroup::Repo(p)) => p
            .file_name()
            .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()),
        GroupId::Auto(AutoGroup::Elsewhere) => "elsewhere".into(),
        GroupId::Auto(AutoGroup::Finished) => "finished · 24h".into(),
    }
}

fn matches(row: &Row, label: Option<&str>, group: &str, needle: &str) -> bool {
    let cwd = row.detail.cwd.to_string_lossy();
    [Some(row.title.as_str()), label, Some(row.now.as_str()), row.meta.branch.as_deref()]
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
            now: node.now.clone(),
            since: node.since,
            prefix: format!("{indent}{}", if last { "└─" } else { "├─" }),
        });
        flatten(out, row, &node.children, &format!("{indent}{}", if last { "   " } else { "│  " }));
    }
    if tree.finished > 0 {
        out.push(Line::Finished { row: row.clone(), count: tree.finished, prefix: format!("{indent}└─") });
    }
}

pub fn render(frame: &mut Frame, app: &App, snap: &Snapshot, user: &UserState, now: At) {
    let lines = layout(snap, user, app);
    let sel = selected(&lines, app);
    let [top, body, foot] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .areas(frame.area());
    frame.render_widget(Paragraph::new(header(snap, top.width)), top);

    let current = sel
        .and_then(|i| lines[i].target())
        .and_then(|t| snap.rows.iter().find(|r| &r.key == t.row()));
    let mut list_area = body;
    if app.detail
        && let Some(row) = current
    {
        let wide = body.width >= 120;
        let [list, pane] = if wide {
            Layout::horizontal([Constraint::Min(0), Constraint::Percentage(40)]).areas(body)
        } else {
            Layout::vertical([Constraint::Min(0), Constraint::Percentage(40)]).areas(body)
        };
        list_area = list;
        render_detail(frame, pane, row, user, if wide { Borders::LEFT } else { Borders::TOP });
    }
    render_list(frame, list_area, &lines, sel, now);
    frame.render_widget(Paragraph::new(footer(app, now)), foot);

    match &app.mode {
        Mode::Help => render_help(frame, body, snap),
        Mode::Move { input, choice, options, .. } => {
            render_picker(frame, body, input, *choice, options);
        }
        _ => {}
    }
}

pub fn dump(app: &App, snap: &Snapshot, user: &UserState, now: At, width: u16) -> String {
    let body: usize = layout(snap, user, app).iter().map(Line::height).sum();
    let height = u16::try_from(body + 2).unwrap_or(u16::MAX);
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("TestBackend is infallible");
    term.draw(|f| render(f, app, snap, user, now)).expect("TestBackend is infallible");
    buffer_text(term.backend().buffer())
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

fn status_style(status: &Status) -> Style {
    match status.rank() {
        Rank::NeedsYou => Style::new().fg(Color::Red),
        Rank::Working => Style::new().fg(Color::Green),
        Rank::Parked => Style::new().fg(Color::Yellow),
        Rank::Idle => Style::new(),
        Rank::Exited => dim(),
    }
}

fn header(snap: &Snapshot, width: u16) -> TLine<'static> {
    let live = snap.rows.iter().filter(|r| matches!(r.key, RowKey::Live { .. })).count();
    let count = |rank| snap.rows.iter().filter(|r| r.status.rank() == rank).count();
    let mut left = vec![
        Span::styled(" agentically", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {live} live")),
    ];
    for (rank, what, color) in
        [(Rank::NeedsYou, "needs you", Color::Red), (Rank::Parked, "parked", Color::Yellow)]
    {
        let n = count(rank);
        if n > 0 {
            left.push(Span::raw(" · "));
            left.push(Span::styled(format!("{n} {what}"), Style::new().fg(color)));
        }
    }
    let mut right = Vec::new();
    if !snap.diagnostics.is_empty() {
        right.push(Span::styled(format!("⚠ {}", snap.diagnostics.len()), Style::new().fg(Color::Yellow)));
        right.push(Span::raw("  "));
    }
    right.push(Span::styled("? help ", dim()));
    spread(left, right, width)
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
            Some(f) if f.until > now => {
                TLine::from(Span::styled(format!(" {}", f.text), Style::new().fg(Color::Yellow)))
            }
            _ => TLine::from(Span::styled(
                " j/k move  l expand  ⏎ open  tab detail  r label  m move  n new  / filter  q quit",
                dim(),
            )),
        },
        Mode::Filter { input } => prompt("/", input, "⏎ keep  esc clear"),
        Mode::Label { input, .. } => prompt("label: ", input, "⏎ save (empty clears)  esc cancel"),
        Mode::Move { input, .. } => {
            prompt("group: ", input, "↑/↓ choose  ⏎ move  - automatic  esc cancel")
        }
        Mode::NewSession { input, .. } => {
            prompt("new session name (optional): ", input, "⏎ launch  esc cancel")
        }
        Mode::Help => TLine::from(Span::styled(" any key closes help", dim())),
    }
}

fn render_list(frame: &mut Frame, area: Rect, lines: &[Line], sel: Option<usize>, now: At) {
    let width = usize::from(area.width);
    let mut out: Vec<TLine> = Vec::new();
    let mut sel_end = 0;
    for (i, line) in lines.iter().enumerate() {
        let is_sel = sel == Some(i);
        draw_line(&mut out, line, is_sel, width, now);
        if is_sel {
            sel_end = out.len();
        }
    }
    let scroll = sel_end.saturating_sub(usize::from(area.height));
    frame.render_widget(Paragraph::new(out).scroll((u16::try_from(scroll).unwrap_or(0), 0)), area);
}

fn draw_line(out: &mut Vec<TLine<'static>>, line: &Line, selected: bool, width: usize, now: At) {
    let marker = Span::styled(if selected { "›" } else { " " }, Style::new().add_modifier(Modifier::BOLD));
    match line {
        Line::Header { group, title, count, needs_you } => {
            let style = if *group == GroupId::Auto(AutoGroup::Finished) {
                dim()
            } else {
                Style::new().add_modifier(Modifier::BOLD)
            };
            let mut right = Vec::new();
            if *needs_you > 0 {
                right.push(Span::styled(format!("▲{needs_you} "), Style::new().fg(Color::Red)));
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
            out.push(TLine::from(spans));
        }
        Line::Row { status, label, title, meta, now: now_line, since, .. } => {
            let exited = status.rank() == Rank::Exited;
            let text = if exited { dim() } else { Style::new() };
            let right = vec![
                Span::styled(meta_text(meta), text),
                Span::styled(format!(" {:>3} ", elapsed(*since, now)), dim()),
            ];
            let mut left = vec![marker, Span::styled(glyph(status), status_style(status)), Span::raw(" ")];
            if let Some(l) = label {
                left.push(Span::styled(l.clone(), text.add_modifier(Modifier::BOLD)));
                left.push(Span::raw("  "));
            }
            let room = width.saturating_sub(spans_width(&left) + spans_width(&right) + 1);
            left.push(Span::styled(truncate(title.as_str(), room), text));
            out.push(spread(left, right, width as u16));
            let second = truncate(now_line.as_str(), width.saturating_sub(INDENT.len()));
            out.push(TLine::from(vec![Span::raw(INDENT), Span::styled(second, dim())]));
        }
        Line::Node { kind, state, label, now: node_now, since, prefix, .. } => {
            let (g, style) = node_glyph(kind, *state);
            let model = match kind {
                NodeKind::Agent { model: Some(m), .. } => format!("{m}  "),
                _ => String::new(),
            };
            let right = vec![Span::styled(format!("{model}{:>3} ", elapsed(*since, now)), dim())];
            let mut left = vec![
                marker,
                Span::raw(format!("{}{prefix} ", &INDENT[1..])),
                Span::styled(g, style),
                Span::raw(" "),
                Span::styled(kind_name(kind).to_owned(), dim()),
                Span::raw("  "),
            ];
            let room = width.saturating_sub(spans_width(&left) + spans_width(&right) + 1);
            let mut text = label.as_str().to_owned();
            if let Some(n) = node_now.as_ref().filter(|n| !n.is_empty()) {
                text = format!("{text}  · {}", n.as_str());
            }
            left.push(Span::raw(truncate(&text, room)));
            out.push(spread(left, right, width as u16));
        }
        Line::Finished { count, prefix, .. } => {
            out.push(TLine::from(Span::styled(format!("{INDENT}{prefix} ✓ {count} finished"), dim())));
        }
    }
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
        NodeKind::Agent { agent_type, .. } => agent_type,
        NodeKind::Shell => "shell",
        NodeKind::Monitor => "monitor",
        NodeKind::Workflow => "workflow",
        NodeKind::Wakeup => "wakeup",
    }
}

fn meta_text(meta: &Meta) -> String {
    let mut parts = Vec::new();
    if meta.remote_control {
        parts.push("⇄".to_owned());
    }
    let mut place: Vec<&str> = Vec::new();
    if !meta.dir_tag.is_empty() {
        place.push(&meta.dir_tag);
    }
    if meta.bg {
        place.push("bg");
    }
    if let Some(b) = meta.branch.as_deref().or(meta.worktree.as_deref()) {
        place.push(b);
    }
    let mut place = place.join(" · ");
    if let Some(pr) = meta.prs.last() {
        let more = if meta.prs.len() > 1 { format!("+{}", meta.prs.len() - 1) } else { String::new() };
        place = format!("{place} #{pr}{more}").trim_start().to_owned();
    }
    if !place.is_empty() {
        parts.push(place);
    }
    match meta.ctx {
        Some(Ctx::Percent(p)) => parts.push(format!("{p}%")),
        Some(Ctx::Tokens(t)) if t >= 1_000_000 => parts.push(format!("{:.1}M", t as f64 / 1e6)),
        Some(Ctx::Tokens(t)) if t >= 1_000 => parts.push(format!("{}k", t / 1000)),
        Some(Ctx::Tokens(t)) => parts.push(t.to_string()),
        None => {}
    }
    if let Some(Cost::Live(c) | Cost::Final(c)) = meta.cost {
        parts.push(format!("${c:.2}"));
    }
    parts.join("  ")
}

fn render_detail(frame: &mut Frame, area: Rect, row: &Row, user: &UserState, border: Borders) {
    let d = &row.detail;
    let mut text = vec![TLine::from(Span::styled(
        row.title.as_str().to_owned(),
        Style::new().add_modifier(Modifier::BOLD),
    ))];
    let mut section = |name: &str, lines: Vec<String>| {
        text.push(TLine::raw(""));
        text.push(TLine::from(Span::styled(name.to_owned(), dim())));
        text.extend(lines.into_iter().map(TLine::raw));
    };
    if let Some(l) = user.entry(&row.user_key).and_then(|e| e.label.clone()) {
        section("label", vec![l]);
    }
    if let Some(p) = &d.last_prompt {
        section("last prompt", vec![p.as_str().to_owned()]);
    }
    if let Some(t) = &d.last_text {
        section("last reply", vec![t.as_str().to_owned()]);
    }
    for (name, items) in [("open tasks", &d.open_tasks), ("pending", &d.pending)] {
        if !items.is_empty() {
            section(name, items.iter().map(|s| format!("· {}", s.as_str())).collect());
        }
    }
    for (name, path) in [("cwd", &d.cwd), ("config dir", &d.config_dir)] {
        if !path.as_os_str().is_empty() {
            section(name, vec![path.display().to_string()]);
        }
    }
    if let Some(s) = &d.session_id {
        section("session", vec![s.0.clone()]);
    }
    if let Some(j) = &d.job_id {
        section("job", vec![j.0.clone()]);
    }
    let block = Block::new().borders(border).border_style(dim()).padding(ratatui::widgets::Padding::horizontal(1));
    frame.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: true }), area);
}

fn render_help(frame: &mut Frame, body: Rect, snap: &Snapshot) {
    let mut text: Vec<TLine> = [
        "j/k ↑/↓   move            g/G   top / bottom",
        "l → space expand          h ←   collapse",
        "⏎         jump to the pane, or attach a bg job",
        "tab p     detail pane     r     label",
        "m         move to group (- automatic)",
        "n         new claude here (same dir, cwd, group)",
        "/         filter          q esc quit",
        "",
        "▲ needs you  ● working  ◐ parked  ? asked a question  ○ idle  · exited",
    ]
    .into_iter()
    .map(TLine::raw)
    .collect();
    text.push(TLine::raw(""));
    if snap.diagnostics.is_empty() {
        text.push(TLine::from(Span::styled("no diagnostics", dim())));
    } else {
        text.push(TLine::from(Span::styled(format!("⚠ {} diagnostics", snap.diagnostics.len()), Style::new().fg(Color::Yellow))));
        for d in &snap.diagnostics {
            text.push(TLine::raw(format!("{:?}  {}  {}", d.source, problem_text(&d.problem), d.path)));
        }
    }
    let area = centered(body, 76, text.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" help ").border_style(dim()).padding(ratatui::widgets::Padding::horizontal(1));
    frame.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), area);
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
            let name = if options.contains(g) { g.0.clone() } else { format!("+ new group \"{}\"", g.0) };
            let style = if i == choice { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            TLine::from(Span::styled(format!(" {name} "), style))
        })
        .collect();
    if text.is_empty() {
        text.push(TLine::from(Span::styled(" type a name to create a group", dim())));
    }
    text.push(TLine::from(Span::styled(" - automatic group", dim())));
    let area = centered(body, 44, text.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(text).block(Block::bordered().title(" move to group ").border_style(dim())), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
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

pub fn glyph(status: &Status) -> &'static str {
    match status {
        Status::NeedsYou(_) => "▲",
        Status::Working => "●",
        Status::Parked(_) => "◐",
        Status::Idle { asked_question: true } => "?",
        Status::Idle { asked_question: false } => "○",
        Status::Exited(_) => "·",
    }
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
    use crate::model::{
        Count, Detail, Diagnostic, DiagSource, JobId, JobOutcome, Need, Node, PaneId, Parked,
        Pid, ProcStart, SessionId, TaskOutcome, WaitingFor,
    };

    const NOW: At = At(1_000_000_000);

    fn key(pid: u32) -> RowKey {
        RowKey::Live { dir: ConfigDir("/h/.claude".into()), pid: Pid(pid), proc_start: ProcStart::new("t") }
    }

    fn parked() -> Status {
        Status::Parked(Parked::new(Count::Exact(2), Count::Exact(0), Count::Exact(1), 0, None).unwrap())
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
            detail: Detail { cwd: format!("/w/{title}").into(), ..Detail::default() },
            open: OpenTarget::Unreachable("not in tmux".into()),
        }
    }

    fn repo(name: &str) -> AutoGroup {
        AutoGroup::Repo(Path::new("/w").join(name))
    }

    fn snap(rows: Vec<Row>) -> Snapshot {
        Snapshot { rows, diagnostics: vec![] }
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
        user.apply(&Op::SetLabel { key: row.user_key.clone(), label: Some(l.into()) });
    }

    fn move_to(user: &mut UserState, row: &Row, g: &str) {
        user.apply(&Op::SetGroup { key: row.user_key.clone(), group: Some(GroupName(g.into())) });
    }

    fn press(app: &mut App, s: &Snapshot, u: &UserState, keys: &[Key]) -> Vec<Effect> {
        keys.iter().flat_map(|k| update(app, s, u, Input::Key(*k), NOW)).collect()
    }

    fn typed(s: &str) -> Vec<Key> {
        s.chars().map(|c| if c == ' ' { Key::Space } else { Key::Char(c) }).collect()
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
        let mut e = row("e", 5, Status::Exited(JobOutcome::Done), 0, AutoGroup::Finished);
        e.key = RowKey::Exited { dir: ConfigDir("/h/.claude".into()), job: JobId("9b3323e1".into()) };
        let mut user = UserState::default();
        user.groups.push(GroupName("empty".into()));
        move_to(&mut user, &c, "zeta");
        move_to(&mut user, &d, "omega");
        move_to(&mut user, &e, "zeta");
        let lines = layout(&snap(vec![e, d, c, b, a]), &user, &App::new(false));
        assert_eq!(headers(&lines), ["zeta:1", "omega:1", "alpha:1", "beta:1", "finished · 24h:1"]);
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
            row("idle", 1, Status::Idle { asked_question: false }, 10, g.clone()),
            row("parked-new", 2, parked(), 90, g.clone()),
            row("asked", 3, Status::Idle { asked_question: true }, 99, g.clone()),
            row("working", 4, Status::Working, 99, g.clone()),
            row("parked-old", 5, parked(), 20, g.clone()),
            row("needs", 6, Status::NeedsYou(Need::Waiting(WaitingFor::Input)), 99, g.clone()),
        ];
        let lines = layout(&snap(rows), &UserState::default(), &App::new(false));
        assert_eq!(titles(&lines), ["needs", "working", "parked-old", "parked-new", "asked", "idle"]);
        assert!(matches!(lines[0], Line::Header { needs_you: 1, count: 6, .. }));
    }

    #[test]
    fn filter_matches_label_branch_cwd_and_group_case_insensitively() {
        let mut a = row("alpha task", 1, Status::Working, 0, repo("one"));
        a.meta.branch = Some("feat/Fees".into());
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
        NodeKind::Agent { agent_type: "Explore".into(), model: Some("sonnet".into()) }
    }

    fn tree_row() -> Row {
        let child = Tree { live: vec![node("a1", agent(), Tree::default())], finished: 2 };
        let mut r = row("fan", 1, parked(), 0, repo("r"));
        r.tree = Tree {
            live: vec![node("a", agent(), child), node("mon", NodeKind::Monitor, Tree::default())],
            finished: 4,
        };
        r
    }

    #[test]
    fn expanded_rows_flatten_depth_first_with_rails_and_finished_counts() {
        let s = snap(vec![tree_row(), row("plain", 2, Status::Working, 0, repo("r"))]);
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
                Line::Row { title, expanded, .. } => format!("row {} {expanded}", title.as_str()),
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
            row("b", 2, Status::Idle { asked_question: false }, 0, g.clone()),
            row("c", 3, Status::Idle { asked_question: false }, 5, g.clone()),
        ]);
        let mut app = App::new(false);
        press(&mut app, &before, &user, &[Key::Char('j')]);
        assert_eq!(cursor_title(&app, &layout(&before, &user, &app)), Some("b"));

        let after = snap(vec![
            row("a", 1, Status::Working, 0, g.clone()),
            row("b", 2, Status::NeedsYou(Need::Waiting(WaitingFor::Input)), 0, g.clone()),
            row("c", 3, Status::Idle { asked_question: false }, 5, g.clone()),
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
            snap(ids.iter().map(|&p| row(&format!("r{p}"), p, Status::Working, i64::from(p), g.clone())).collect())
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
        assert_eq!(update(&mut app, &s, &user, Input::Done(Outcome::Jumped), NOW), [Effect::Quit]);
    }

    #[test]
    fn a_failed_jump_flashes_and_keeps_the_popup_open() {
        let s = snap(vec![pane_row()]);
        let user = UserState::default();
        let mut app = App::new(true);
        let fx = update(&mut app, &s, &user, Input::Done(Outcome::Failed("pane gone".into())), NOW);
        assert_eq!(fx, []);
        assert!(dump(&app, &s, &user, NOW, 100).contains("pane gone"));
    }

    #[test]
    fn the_persistent_window_stays_open_after_a_jump() {
        let s = snap(vec![pane_row()]);
        let mut app = App::new(false);
        assert_eq!(update(&mut app, &s, &UserState::default(), Input::Done(Outcome::Jumped), NOW), []);
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
        r.open = OpenTarget::Attach { dir: dir.clone(), job: JobId("9b3323e1".into()), title: "fees".into() };
        let s = snap(vec![r]);
        let fx = press(&mut App::new(false), &s, &UserState::default(), &[Key::Enter]);
        assert_eq!(fx, [Effect::Attach { dir, job: JobId("9b3323e1".into()), title: "fees".into() }]);
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
        };
        assert_eq!(press(&mut app, &s, &user, &[Key::Char('n'), Key::Enter]), [expect(None)]);
        let mut keys = vec![Key::Char('n')];
        keys.extend(typed("fix the  bug "));
        keys.push(Key::Enter);
        assert_eq!(press(&mut app, &s, &user, &keys), [expect(Some("fix the  bug"))]);
        assert_eq!(app.mode, Mode::Normal);

        let spawned = Outcome::Spawned { session: SessionId("u".into()), group: Some(GroupName("side".into())) };
        assert_eq!(
            update(&mut app, &s, &user, Input::Done(spawned), NOW),
            [Effect::Commit(Op::SetGroup {
                key: UserKey::Session(SessionId("u".into())),
                group: Some(GroupName("side".into()))
            })]
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
        let fx = press(&mut app, &s, &user, &[Key::Char('r'), Key::Char('c'), Key::Enter]);
        assert_eq!(fx, [Effect::Commit(Op::SetLabel { key: k.clone(), label: Some("abc".into()) })]);
        let fx = press(&mut app, &s, &user, &[Key::Char('r'), Key::Backspace, Key::Backspace, Key::Enter]);
        assert_eq!(fx, [Effect::Commit(Op::SetLabel { key: k.clone(), label: None })]);
        assert_eq!(press(&mut app, &s, &user, &[Key::Char('r'), Key::Char('z'), Key::Esc]), []);
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
            vec![Effect::Commit(Op::SetGroup { key: k.clone(), group: g.map(|g| GroupName(g.into())) })]
        };
        assert_eq!(press(&mut app, &s, &user, &[Key::Char('m'), Key::Down, Key::Enter]), set(Some("side")));
        assert_eq!(press(&mut app, &s, &user, &[Key::Char('m'), Key::Char('-')]), set(None));
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
        press(&mut app, &s, &user, &[Key::Char('/'), Key::Char('f'), Key::Char('e')]);
        assert_eq!(titles(&layout(&s, &user, &app)), ["fees"]);
        press(&mut app, &s, &user, &[Key::Enter]);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(titles(&layout(&s, &user, &app)), ["fees"]);
        assert_eq!(press(&mut app, &s, &user, &[Key::Esc]), []);
        assert_eq!(titles(&layout(&s, &user, &app)), ["fees", "docs"]);
        assert_eq!(press(&mut app, &s, &user, &[Key::Esc]), [Effect::Quit]);

        let mut app = App::new(false);
        press(&mut app, &s, &user, &[Key::Char('/'), Key::Char('d'), Key::Esc]);
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
        assert_eq!(press(&mut app, &s, &user, &[Key::Char('q')]), [Effect::Quit]);
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

    #[test]
    fn elapsed_formats_each_unit_boundary() {
        let at = |s: i64| elapsed(At(NOW.0 - s * 1000), NOW);
        assert_eq!([at(42), at(60), at(3599), at(3600), at(86_400), at(-5)], ["42s", "1m", "59m", "1h", "1d", "0s"]);
    }

    #[test]
    fn test_backend_render_smoke() {
        let mut needs = row("Fix token refresh race", 1, Status::NeedsYou(Need::Permission { tool: Some("Bash".into()) }), NOW.0 - 180_000, repo("jnj-web"));
        needs.now = SafeText::ours("permission prompt · Bash");
        needs.meta = Meta { branch: Some("feat/auth-race".into()), prs: vec![4812], ctx: Some(Ctx::Tokens(412_000)), ..Meta::default() };
        let mut fan = tree_row();
        fan.tree.live[1].state = NodeState::Finished(TaskOutcome::Completed);
        let mut s = snap(vec![needs, fan, row("quiet", 3, Status::Idle { asked_question: true }, 0, AutoGroup::Elsewhere)]);
        s.diagnostics.push(Diagnostic { source: DiagSource::Registry, problem: Problem::Unparsable, path: "~/.claude/sessions/1.json".into() });
        let mut user = UserState::default();
        label(&mut user, &s.rows[0], "auth-race");
        let mut app = App::new(false);
        app.expanded.insert(key(1));
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &app, &s, &user, NOW)).unwrap();
        let text = buffer_text(term.backend().buffer());
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with(" agentically  3 live · 1 needs you · 1 parked"), "{text}");
        assert!(lines[0].ends_with("⚠ 1  ? help"), "{text}");
        assert!(lines[1].starts_with(" jnj-web ─") && lines[1].ends_with("▲1 1"), "{text}");
        assert!(lines[2].starts_with("›▲ auth-race  Fix token refresh race"), "{text}");
        assert!(lines[2].ends_with("feat/auth-race #4812  412k  3m"), "{text}");
        assert_eq!(lines[3], "     permission prompt · Bash");
        assert!(text.contains("│  ├─ ● Explore  a1"), "{text}");
        assert!(text.contains("├─ ✓ monitor  mon"), "{text}");
        assert!(text.contains("     └─ ✓ 4 finished"), "{text}");
        assert!(text.contains(" ? quiet"), "{text}");
        assert!(lines[29].starts_with(" j/k move"), "{text}");
    }
}
