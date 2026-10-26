//! The skills view: one row per skill, one column per account. agentty only reads the folders;
//! Enter starts a `share-skill` agent that checks the skill is fit to share and links it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::{App, Key, Mode, dim, truncate};
use crate::derive::roomiest;
use crate::model::{At, ConfigDir, Effect, Op, Snapshot};
use crate::skills::{Cell, Inventory, Skill};
use crate::user_state::UserState;

pub const FOOTER: &str = " j/k move  ⏎ share  x keep local  r rescan  s esc sessions  q quit";
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

    /// The header line, the grid, and the shared-folder note.
    pub fn height(&self) -> usize {
        self.inv.skills.len() + 1 + usize::from(!shared_folders(&self.inv).is_empty())
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

pub fn render(frame: &mut Frame, area: Rect, app: &App, view: &SkillsView, user: &UserState) {
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
        .clamp(5, NAME_MAX);
    let mut head = vec![Span::styled(format!("  {:<name_w$}", "skill"), dim())];
    for n in &names {
        head.push(Span::styled(format!("  {n}"), dim()));
    }
    let mut out = vec![TLine::from(head)];

    let order = order(inv, user);
    let at = view.at(&order);
    for (pos, &i) in order.iter().enumerate() {
        let skill = &inv.skills[i];
        let need = need(skill, user);
        let base = if need == Need::Local {
            dim()
        } else {
            Style::new()
        };
        let mark = if at == Some(pos) { "›" } else { " " };
        let mut spans = vec![Span::styled(
            format!("{mark} {:<name_w$}", truncate(&skill.name, NAME_MAX)),
            base,
        )];
        let copies = skill.spread().copies;
        for (cell, n) in skill.cells.iter().zip(&names) {
            let (glyph, style) = cell_glyph(cell, copies);
            let pad = n.width().saturating_sub(1);
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                glyph,
                if need == Need::Local { dim() } else { style },
            ));
            spans.push(Span::raw(" ".repeat(pad)));
        }
        let (text, style) = summary(skill, user, &names);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(text, style));
        out.push(TLine::from(spans));
    }
    let note = shared_folders(inv);
    if !note.is_empty() {
        let groups: Vec<String> = note
            .iter()
            .map(|g| {
                g.iter()
                    .map(|&i| names[i].as_str())
                    .collect::<Vec<_>>()
                    .join(" = ")
            })
            .collect();
        out.push(TLine::from(Span::styled(
            format!(
                "  one skills folder: {}   ● copy  → link  · missing  ✗ broken",
                groups.join(", ")
            ),
            dim(),
        )));
    }
    let sel_end = at.map_or(0, |p| p + 2);
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
        Cell::Has { linked: true, .. } => ("→", copy),
        Cell::Missing => ("·", dim()),
        Cell::Broken => ("✗", Style::new().fg(Color::Red)),
    }
}

fn summary(skill: &Skill, user: &UserState, names: &[String]) -> (String, Style) {
    if user.local_skills.contains(&skill.name) {
        return ("keep local".into(), dim());
    }
    let spread = skill.spread();
    let list = |ix: &[usize]| {
        ix.iter()
            .map(|&i| names[i].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    if !spread.broken.is_empty() {
        parts.push(format!("broken link on {}", list(&spread.broken)));
    }
    if spread.copies > 1 {
        parts.push(if skill.differ {
            format!("{} copies, SKILL.md differs", spread.copies)
        } else {
            format!("{} identical copies", spread.copies)
        });
    }
    if !spread.missing.is_empty() {
        parts.push(format!("missing on {}", list(&spread.missing)));
    }
    match parts.is_empty() {
        true => ("shared".into(), Style::new().fg(Color::Green)),
        false if !spread.broken.is_empty() => (parts.join(" · "), Style::new().fg(Color::Red)),
        false => (parts.join(" · "), Style::new().fg(Color::Yellow)),
    }
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
