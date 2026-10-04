//! agentically: a read-only, tmux-native dashboard over every live Claude Code session.
//!
//! The thin shell. One thread owns all state (`World`, `Store`, `App`, `Snapshot`); two feeder
//! threads only send `Msg`s: crossterm input, and the notify doorbell. A 1 s tick bounds
//! staleness for liveness, transcript polling and time-in-status.

// Scaffold phase only: bodies are `todo!()`, so every item is unused. Remove once implemented.
#![allow(dead_code, unused_variables)]

mod derive;
mod jobs;
mod model;
mod registry;
mod sidecars;
mod subagents;
mod transcript;
mod ui;
mod user_state;
mod world;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use model::{ConfigDir, Effect, JobId, Outcome, PaneId, SessionId};
use world::World;

/// Everything the loop wakes up for.
enum Msg {
    Term(ratatui::crossterm::event::Event),
    /// A watched path changed. Payload deliberately ignored: refresh re-stats everything.
    Doorbell,
}

struct Args {
    popup: bool,
}

const TICK: Duration = Duration::from_secs(1);
/// Coalesce doorbells: at most one refresh per this interval.
const MIN_REFRESH: Duration = Duration::from_millis(250);

fn main() -> anyhow::Result<()> {
    todo!(
        "parse_args; Roots::from_env; World::new(roots, Box::new(RealHost), ReadBudget::default()); \
         Store::load(default_path); App::new(popup); channel; spawn_input; spawn_doorbell(world.watch_roots() \
         + store.path().parent()); ratatui::init; run; ratatui::restore"
    )
}

/// The loop:
/// ```text
/// loop {
///     msg = rx.recv_timeout(until next tick)
///     drain rx (coalesce doorbells; handle every key in order)
///     if tick, or doorbell and MIN_REFRESH elapsed:
///         world.refresh(Wake::Tick | Wake::Doorbell)
///         store.reload_if_changed()
///         snap = derive::snapshot(world.sessions(), world.exited(now), world.diagnostics(), now)
///         ops = user_state::rebind(store.state(), snap.rows.filter_map(Row::live_anchor))
///         for op in ops: store.commit(op)      // idempotent, no-op writes skipped
///         if watch_roots changed: re-watch
///     for key: effects = ui::update(&mut app, &snap, store.state(), Input::Key(k), now)
///     for e in effects: outcome = execute(e, &mut store); feed back via ui::update(Input::Done)
///     if quit: break
///     terminal.draw(|f| ui::render(f, &app, &snap, store.state(), now))
/// }
/// ```
fn run(
    terminal: &mut ratatui::DefaultTerminal,
    rx: mpsc::Receiver<Msg>,
    world: &mut World,
    store: &mut user_state::Store,
    app: &mut ui::App,
) -> anyhow::Result<()> {
    todo!()
}

/// What to run inside a new tmux window. Ids are validated at parse (JobId 8 hex, SessionId a
/// uuid), and tmux 3.4+ executes `shell-command argument ...` directly without `sh -c`, so no
/// string is ever shell-quoted here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeRun {
    Attach(JobId),
    New { session: SessionId, name: Option<String> },
}

/// Every tmux invocation, as data, so the exact command lines are unit-tested against `argv()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxCmd {
    /// Existence check: `list-panes -t %N` exits 1 when the pane is gone (never `display -p`,
    /// gotcha 31).
    ListPane(PaneId),
    SwitchClient(PaneId),
    /// Always carries `-e CLAUDE_CONFIG_DIR=<dir>`: `tmux new-window` skips the user's zsh
    /// `claude()` wrapper (gotcha 32).
    NewWindow { name: String, cwd: Option<PathBuf>, config_dir: ConfigDir, run: ClaudeRun },
}

impl TmuxCmd {
    /// The argv after `tmux`:
    /// - `ListPane(p)`      -> `["list-panes", "-t", p]`
    /// - `SwitchClient(p)`  -> `["switch-client", "-t", p]`
    /// - `NewWindow{..}`    -> `["new-window", "-n", name, ("-c", cwd)?, "-e", "CLAUDE_CONFIG_DIR=<dir>",
    ///                          "claude", "attach", job]` or
    ///                         `[..., "claude", "--session-id", sid, ("--name", name)?]`
    pub fn argv(&self) -> Vec<String> {
        todo!()
    }
}

/// Execute one effect. The only place that spawns tmux or claude. Returns `None` for `Quit`
/// (the loop exits) and an `Outcome` otherwise.
/// - `Jump(p)`        -> ListPane(p) (rc != 0 => Failed("pane gone")); SwitchClient(p) => Jumped
/// - `Attach{..}`     -> NewWindow{run: Attach(job)} => Attached
/// - `NewSession{..}` -> sid = mint_uuid_v4(); NewWindow{run: New{sid, name}} => Spawned{sid, group}
/// - `Commit(op)`     -> store.commit(&op) => Committed
/// - Outside tmux (`$TMUX` unset): Jump/Attach/NewSession => Failed("not inside tmux")
fn execute(effect: Effect, store: &mut user_state::Store) -> Option<Outcome> {
    todo!()
}

/// A v4 uuid from /dev/urandom (no uuid crate). The only randomness in the app.
fn mint_uuid_v4() -> std::io::Result<SessionId> {
    todo!()
}

/// `--popup` and `--help`. Nothing else in v1.
fn parse_args(args: impl Iterator<Item = String>) -> anyhow::Result<Args> {
    todo!()
}

/// A thread blocking on `event::read()`, forwarding each event as `Msg::Term`.
fn spawn_input(tx: mpsc::Sender<Msg>) {
    todo!()
}

/// One recursive watch per root (`World::watch_roots` plus the state file's dir); the callback
/// only sends `Msg::Doorbell`. Returned so the loop can re-watch when the roots change.
fn spawn_doorbell(
    tx: mpsc::Sender<Msg>,
    roots: Vec<PathBuf>,
) -> notify::Result<notify::RecommendedWatcher> {
    todo!()
}
