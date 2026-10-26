//! The skills view: one row per skill, one column per account. agentty only reads the folders;
//! Enter starts a `share-skill` agent that checks the skill is fit to share and links it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::{App, Key, Mode, band, dim, truncate};
use crate::derive::roomiest;
use crate::model::{At, ConfigDir, Effect, Op, Snapshot};
use crate::skills::{Cell, Inventory, Skill};
use crate::user_state::UserState;

pub const KEYS: &[(&str, &str)] = &[
    ("⏎", "share"),
    ("x", "keep local"),
    ("r", "rescan"),
    ("esc", "sessions"),
    ("?", "help"),
];
const NAME_MAX: usize = 32;

#[derive(Debug)]
pub struct SkillsView {
    pub inv: Inventory,
    /// The selected skill's name, so the cursor stays on it across a rescan or re-sort.
    cursor: Option<String>,
}

impl SkillsView {
    pub fn new(inv: Inventory) -> SkillsView {
        SkillsView { inv, cursor: None }
    }

    pub fn rescanned(self, inv: Inventory) -> SkillsView {
        SkillsView {
            inv,
            cursor: self.cursor,
        }
    }

    /// The selected position in `order`: the cursor's skill, else the first row.
    fn at(&self, order: &[usize]) -> Option<usize> {
        let named = self
            .cursor
            .as_ref()
            .and_then(|name| order.iter().position(|&i| &self.inv.skills[i].name == name));
        named.or((!order.is_empty()).then_some(0))
    }
}

/// Skills that need something first, then shared ones, then those kept local; by name within.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Need {
    Act,
    Shared,
    Local,
}

fn need(skill: &Skill, user: &UserState) -> Need {
    if user.local_skills.contains(&skill.name) {
        Need::Local
    } else if skill.spread().shared() {
        Need::Shared
    } else {
        Need::Act
    }
}

pub fn order(inv: &Inventory, user: &UserState) -> Vec<usize> {
    let mut out: Vec<usize> = (0..inv.skills.len()).collect();
    out.sort_by_key(|&i| (need(&inv.skills[i], user), &inv.skills[i].name));
    out
}

pub fn keys(app: &mut App, snap: &Snapshot, user: &UserState, key: Key, now: At) -> Vec<Effect> {
    let Some(view) = app.skills.as_mut() else {
        return vec![];
    };
    let order = order(&view.inv, user);
    let at = view.at(&order);
    let mut place = |i: usize| view.cursor = Some(view.inv.skills[order[i]].name.clone());
    match key {
        Key::Char('j') | Key::Down => {
            if let Some(i) = at.filter(|&i| i + 1 < order.len()) {
                place(i + 1);
            }
        }
        Key::Char('k') | Key::Up => {
            if let Some(i) = at.filter(|&i| i > 0) {
                place(i - 1);
            }
        }
        Key::Char('g') if !order.is_empty() => place(0),
        Key::Char('G') if !order.is_empty() => place(order.len() - 1),
        Key::Char('r') => return vec![Effect::ScanSkills(view.inv.accounts.clone())],
        Key::Char('s') | Key::Esc => app.skills = None,
        Key::Char('q') => return vec![Effect::Quit],
        Key::Char('?') => app.mode = Mode::Help,
        Key::Char('x') => {
            if let Some(i) = at {
                let name = view.inv.skills[order[i]].name.clone();
                let local = !user.local_skills.contains(&name);
                return vec![Effect::Commit(Op::SetSkillLocal { name, local })];
            }
        }
        Key::Enter => {
            if let Some(i) = at {
                return share(app, snap, user, order[i], now);
            }
        }
        _ => {}
    }
    vec![]
}

/// Asks which account runs the `share-skill` agent, with the prompt ready.
fn share(app: &mut App, snap: &Snapshot, user: &UserState, i: usize, now: At) -> Vec<Effect> {
    let Some(view) = &app.skills else {
        return vec![];
    };
    let skill = &view.inv.skills[i];
    let config = &app.scope.config;
    let prompt = match (need(skill, user), config.shared_skills.as_deref()) {
        (Need::Act, Some(shared)) => Ok(view
            .inv
            .share_prompt(skill, shared, |d| config.account_name(d))),
        (Need::Act, None) => {
            Err("set [skills] shared in ~/.config/agentty/config.toml first".into())
        }
        (Need::Shared, _) => Err(format!("{} is already shared everywhere", skill.name)),
        (Need::Local, _) => Err("kept local: x to share it again".into()),
    };
    let prompt = match prompt {
        Ok(p) => p,
        Err(msg) => {
            app.flash(msg, now);
            return vec![];
        }
    };
    let accounts: Vec<ConfigDir> = match app.workspace() {
        Some(w) => w.accounts.clone(),
        None => view.inv.accounts.clone(),
    };
    app.mode = Mode::NewSession {
        choice: roomiest(&accounts, &snap.limits, now),
        accounts,
        cwd: app.scope.cwd.clone(),
        group: None,
        input: format!("share {}", skill.name),
        prompt: Some(prompt),
    };
    vec![]
}

/// The view's lines: account names over the columns, then a section per need, each skill on
/// one line with its plain-words problem, and a legend. Also returns how many lines reach to
/// the end of the selected skill, for scrolling.
pub fn lines(
    app: &App,
    view: &SkillsView,
    user: &UserState,
    width: usize,
) -> (Vec<TLine<'static>>, usize) {
    let inv = &view.inv;
    let config = &app.scope.config;
    let names: Vec<String> = inv
        .accounts
        .iter()
        .map(|d| config.account_name(d))
        .collect();
    let name_w = inv
        .skills
        .iter()
        .map(|s| s.name.width())
        .max()
        .unwrap_or(0)
        .clamp(5, NAME_MAX)
        + 2;
    let col_w: Vec<usize> = names.iter().map(|n| n.width().max(1) + 2).collect();
    let bold = Style::new().add_modifier(Modifier::BOLD);

    let mut head = vec![Span::raw(format!("   {:<name_w$}", ""))];
    for (n, w) in names.iter().zip(&col_w) {
        head.push(Span::styled(format!("{n:<w$}"), dim()));
    }
    let mut out = vec![TLine::from(head)];

    let order = order(inv, user);
    let at = view.at(&order).map(|p| order[p]);
    let mut last = None;
    let mut sel_end = 0;
    for &i in &order {
        let skill = &inv.skills[i];
        let need = need(skill, user);
        if last != Some(need) {
            last = Some(need);
            let (title, style) = match need {
                Need::Act => ("Needs sharing", bold.fg(Color::Yellow)),
                Need::Shared => ("Shared everywhere", bold.fg(Color::Green)),
                Need::Local => ("Kept local", dim().add_modifier(Modifier::BOLD)),
            };
            let n = order
                .iter()
                .filter(|&&j| self::need(&inv.skills[j], user) == need)
                .count();
            out.push(TLine::raw(""));
            out.push(TLine::from(vec![
                Span::styled(format!(" {title}"), style),
                Span::styled(format!("  {n}"), dim()),
            ]));
        }
        let selected = at == Some(i);
        let name_style = match (selected, need) {
            (true, _) => bold,
            (false, Need::Local) => dim(),
            (false, _) => Style::new(),
        };
        let mut spans = vec![
            Span::styled(
                if selected { " ▌ " } else { "   " },
                Style::new().fg(Color::Yellow),
            ),
            Span::styled(
                format!("{:<name_w$}", truncate(&skill.name, NAME_MAX)),
                name_style,
            ),
        ];
        let copies = skill.spread().copies;
        for (cell, w) in skill.cells.iter().zip(&col_w) {
            let (glyph, style) = cell_glyph(cell, copies);
            let style = if need == Need::Local { dim() } else { style };
            spans.push(Span::styled(format!("{glyph:<w$}"), style));
        }
        if need != Need::Shared {
            spans.push(Span::styled(
                problem(skill, user, &names),
                if need == Need::Local {
                    dim()
                } else {
                    Style::new()
                },
            ));
        }
        let line = TLine::from(spans);
        out.push(if selected { band(line, width) } else { line });
        if selected && let Some(hint) = hint(need) {
            let line = TLine::from(vec![
                Span::raw(" ".repeat(3 + name_w)),
                Span::styled(hint, dim()),
            ]);
            out.push(band(line, width));
        }
        if selected {
            sel_end = out.len();
        }
    }

    out.push(TLine::raw(""));
    let mut legend = "   ● own copy   → linked   · missing   ✗ broken link".to_owned();
    for g in shared_folders(inv) {
        let joined: Vec<&str> = g.iter().map(|&i| names[i].as_str()).collect();
        legend.push_str(&format!(
            "      {} share one skills folder",
            joined.join(" and ")
        ));
    }
    out.push(TLine::from(Span::styled(legend, dim())));
    (out, sel_end)
}

/// What Enter or x does to the selected skill.
fn hint(need: Need) -> Option<&'static str> {
    match need {
        Need::Act => {
            Some("⏎ starts a share-skill agent that keeps one copy and links it into every account")
        }
        Need::Shared => None,
        Need::Local => Some("x shares it again"),
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &App, view: &SkillsView, user: &UserState) {
    let (out, sel_end) = lines(app, view, user, usize::from(area.width));
    let scroll = sel_end.saturating_sub(usize::from(area.height));
    frame.render_widget(
        Paragraph::new(out).scroll((u16::try_from(scroll).unwrap_or(0), 0)),
        area,
    );
}

fn cell_glyph(cell: &Cell, copies: usize) -> (&'static str, Style) {
    let copy = if copies > 1 {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    };
    match cell {
        Cell::Has { linked: false, .. } => ("●", copy),
        Cell::Has { linked: true, .. } => ("→", dim()),
        Cell::Missing => ("·", dim()),
        Cell::Broken => ("✗", Style::new().fg(Color::Red)),
    }
}

/// What's wrong with a skill, in a sentence that names the accounts.
fn problem(skill: &Skill, user: &UserState, names: &[String]) -> String {
    if user.local_skills.contains(&skill.name) {
        return "Kept on these accounts only".into();
    }
    let spread = skill.spread();
    let list = |ix: &[usize]| {
        ix.iter()
            .map(|&i| names[i].as_str())
            .collect::<Vec<_>>()
            .join(" and ")
    };
    let mut parts = Vec::new();
    if !spread.broken.is_empty() {
        parts.push(format!("broken link on {}", list(&spread.broken)));
    }
    if spread.copies > 1 {
        parts.push(if skill.differ {
            format!("{} separate copies that differ", spread.copies)
        } else {
            format!("{} identical copies", spread.copies)
        });
    }
    if !spread.missing.is_empty() {
        parts.push(format!("missing on {}", list(&spread.missing)));
    }
    let text = parts.join("; ");
    let mut c = text.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Groups of two or more accounts whose `skills/` is one folder.
fn shared_folders(inv: &Inventory) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    for (i, f) in inv.folders.iter().enumerate() {
        let Some(f) = f else { continue };
        match out
            .iter_mut()
            .find(|g| inv.folders[g[0]].as_ref() == Some(f))
        {
            Some(g) => g.push(i),
            None => out.push(vec![i]),
        }
    }
    out.retain(|g| g.len() > 1);
    out
}
