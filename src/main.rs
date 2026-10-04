//! agentically: a read-only, tmux-native dashboard over every live Claude Code session.
//!
//! The thin shell. One thread owns all state (`World`, `Store`, `App`, `Snapshot`); two feeder
//! threads only send `Msg`s: crossterm input, and the notify doorbell. A 1 s tick bounds
//! staleness for liveness, transcript polling and time-in-status.

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

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Context;
use notify::Watcher;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};

use model::{
    ConfigDir, DiagSource, Diagnostic, Effect, JobId, Outcome, PaneId, Problem, Row, SessionId,
    Snapshot,
};
use world::{ReadBudget, RealHost, Roots, Wake, World};

/// Everything the loop wakes up for.
enum Msg {
    Term(Event),
    /// A watched path changed. Payload deliberately ignored: refresh re-stats everything.
    Doorbell,
}

#[derive(Debug, Default, PartialEq)]
struct Args {
    popup: bool,
    dump: bool,
    help: bool,
    state: Option<PathBuf>,
}

const TICK: Duration = Duration::from_secs(1);
/// Coalesce doorbells: at most one refresh per this interval.
const MIN_REFRESH: Duration = Duration::from_millis(250);
/// The read budget defers first-sight windows across ticks; `--dump` keeps refreshing until
/// nothing is deferred so one frame shows every session fully read.
const DUMP_MAX_REFRESHES: usize = 20;

/// `--dump` width: the e2e scratch tmux session is 160 columns.
const DUMP_WIDTH: u16 = 160;

const USAGE: &str = "usage: agentically [--popup] [--state <path>] [--dump]

  --popup         quit after a successful jump (for tmux display-popup -E)
  --state <path>  user state file (default ~/.config/agentically/state.toml)
  --dump          print one frame as plain text and exit (no TTY needed)
";

fn main() -> anyhow::Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    if args.help {
        print!("{USAGE}");
        return Ok(());
    }
    let roots = Roots::from_env()?;
    let state_path =
        args.state.unwrap_or_else(|| user_state::Store::default_path(&roots.home));
    let mut world = World::new(roots, Box::new(RealHost), ReadBudget::default());
    let mut store = user_state::Store::load(state_path)?;
    let mut app = ui::App::new(args.popup);

    if args.dump {
        let mut snap = Snapshot::default();
        for _ in 0..DUMP_MAX_REFRESHES {
            world.refresh(Wake::Tick);
            snap = snapshot(&world);
            if !snap.diagnostics.iter().any(|d| d.problem == Problem::Deferred) {
                break;
            }
        }
        print!("{}", ui::dump(&app, &snap, store.state(), world.now(), DUMP_WIDTH));
        for d in &snap.diagnostics {
            println!("⚠ {:?} {:?} {}", d.source, d.problem, d.path);
        }
        return Ok(());
    }

    let (tx, rx) = mpsc::channel();
    spawn_input(tx.clone());
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, tx, rx, &mut world, &mut store, &mut app);
    ratatui::restore();
    result
}

fn snapshot(world: &World) -> Snapshot {
    let now = world.now();
    derive::snapshot(world.sessions(), world.exited(now).into_iter(), world.diagnostics(), now)
}

/// The loop: wait for a message or the next deadline; handle keys in order against the frame
/// the user saw; refresh on the tick, or on a doorbell at most once per `MIN_REFRESH`; after a
/// refresh, rebind anchors and commit the resulting ops (no-ops write nothing); re-watch when
/// the set of existing watch roots changed; draw.
fn run(
    terminal: &mut ratatui::DefaultTerminal,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    world: &mut World,
    store: &mut user_state::Store,
    app: &mut ui::App,
) -> anyhow::Result<()> {
    let mut snap = Snapshot::default();
    let mut watched: Vec<PathBuf> = Vec::new();
    let mut _watcher: Option<notify::RecommendedWatcher> = None;
    let mut next_tick = Instant::now();
    let mut last_refresh = Instant::now() - MIN_REFRESH;
    let mut doorbell = false;
    loop {
        let deadline =
            if doorbell { next_tick.min(last_refresh + MIN_REFRESH) } else { next_tick };
        let first = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(msg) => Some(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        for msg in first.into_iter().chain(rx.try_iter()) {
            match msg {
                Msg::Doorbell => doorbell = true,
                Msg::Term(Event::Key(k))
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    return Ok(());
                }
                Msg::Term(Event::Key(k)) => {
                    if let Some(key) = ui::key_of(&k)
                        && dispatch(app, &snap, store, ui::Input::Key(key), world.now())
                    {
                        return Ok(());
                    }
                }
                Msg::Term(_) => {}
            }
        }

        let now = Instant::now();
        let wake = if now >= next_tick {
            Some(Wake::Tick)
        } else if doorbell && now >= last_refresh + MIN_REFRESH {
            Some(Wake::Doorbell)
        } else {
            None
        };
        if let Some(wake) = wake {
            world.refresh(wake);
            let mut problems = Vec::new();
            if store.reload_if_changed().is_err() {
                problems.push(Problem::Unreadable);
            }
            snap = snapshot(world);
            let ops = user_state::rebind(store.state(), snap.rows.iter().filter_map(Row::live_anchor));
            if ops.iter().any(|op| store.commit(op).is_err()) {
                problems.push(Problem::Unavailable);
            }
            snap.diagnostics.extend(problems.into_iter().map(|problem| Diagnostic {
                source: DiagSource::UserState,
                problem,
                path: store.path().display().to_string(),
            }));
            let roots = existing_watch_roots(world, store.path());
            if roots != watched {
                _watcher = spawn_doorbell(tx.clone(), roots.clone()).ok();
                watched = roots;
            }
            doorbell = false;
            last_refresh = now;
            if wake == Wake::Tick {
                let deferred = snap.diagnostics.iter().any(|d| d.problem == Problem::Deferred);
                // Cold start reads windows a budget at a time; paint each batch, then continue at once.
                next_tick = if deferred { now } else { now + TICK };
            }
        }
        terminal.draw(|f| ui::render(f, app, &snap, store.state(), world.now()))?;
    }
}

/// Run the reducer, then every effect it asks for, feeding each outcome back. Returns true when
/// an effect was `Quit`.
fn dispatch(
    app: &mut ui::App,
    snap: &Snapshot,
    store: &mut user_state::Store,
    input: ui::Input,
    now: model::At,
) -> bool {
    let mut queue: VecDeque<Effect> = ui::update(app, snap, store.state(), input, now).into();
    while let Some(effect) = queue.pop_front() {
        match execute(effect, store) {
            None => return true,
            Some(outcome) => {
                queue.extend(ui::update(app, snap, store.state(), ui::Input::Done(outcome), now));
            }
        }
    }
    false
}

/// Only roots that exist are watched, so a dir that appears later changes this set and gets
/// watched on the next refresh.
fn existing_watch_roots(world: &World, state_file: &Path) -> Vec<PathBuf> {
    world
        .watch_roots()
        .into_iter()
        .chain(state_file.parent().map(Path::to_path_buf))
        .filter(|p| p.exists())
        .collect()
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
    /// - `NewWindow{..}`    -> `["new-window", "-n", name, ("-c", cwd)?, "-e",
    ///   "CLAUDE_CONFIG_DIR=<dir>", "claude", "attach", job]` or
    ///   `[..., "claude", "--session-id", sid, ("--name", name)?]`
    pub fn argv(&self) -> Vec<String> {
        match self {
            TmuxCmd::ListPane(p) => vec!["list-panes".into(), "-t".into(), p.as_str().into()],
            TmuxCmd::SwitchClient(p) => {
                vec!["switch-client".into(), "-t".into(), p.as_str().into()]
            }
            TmuxCmd::NewWindow { name, cwd, config_dir, run } => {
                let mut v = vec!["new-window".into(), "-n".into(), name.clone()];
                if let Some(cwd) = cwd {
                    v.extend(["-c".into(), cwd.display().to_string()]);
                }
                v.extend([
                    "-e".into(),
                    format!("CLAUDE_CONFIG_DIR={}", config_dir.0.display()),
                    "claude".into(),
                ]);
                match run {
                    ClaudeRun::Attach(job) => v.extend(["attach".into(), job.0.clone()]),
                    ClaudeRun::New { session, name } => {
                        v.extend(["--session-id".into(), session.0.clone()]);
                        if let Some(name) = name {
                            v.extend(["--name".into(), name.clone()]);
                        }
                    }
                }
                v
            }
        }
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
    let failed = |what: &str| Outcome::Failed(what.to_owned());
    Some(match effect {
        Effect::Quit => return None,
        Effect::Commit(op) => match store.commit(&op) {
            Ok(()) => Outcome::Committed,
            Err(e) => Outcome::Failed(format!("could not save state: {e}")),
        },
        _ if std::env::var_os("TMUX").is_none() => failed("not inside tmux"),
        Effect::Jump(pane) => {
            if !tmux(&TmuxCmd::ListPane(pane.clone())) {
                Outcome::Failed(format!("pane {} is gone", pane.as_str()))
            } else if !tmux(&TmuxCmd::SwitchClient(pane)) {
                failed("tmux switch-client failed")
            } else {
                Outcome::Jumped
            }
        }
        Effect::Attach { dir, job, title } => {
            let cmd =
                TmuxCmd::NewWindow { name: title, cwd: None, config_dir: dir, run: ClaudeRun::Attach(job) };
            if tmux(&cmd) { Outcome::Attached } else { failed("tmux new-window failed") }
        }
        Effect::NewSession { dir, cwd, group, name } => match mint_uuid_v4() {
            Err(e) => Outcome::Failed(format!("could not mint a session id: {e}")),
            Ok(session) => {
                let cmd = TmuxCmd::NewWindow {
                    name: name.clone().unwrap_or_else(|| "claude".into()),
                    cwd: Some(cwd),
                    config_dir: dir,
                    run: ClaudeRun::New { session: session.clone(), name },
                };
                if tmux(&cmd) {
                    Outcome::Spawned { session, group }
                } else {
                    failed("tmux new-window failed")
                }
            }
        },
    })
}

fn tmux(cmd: &TmuxCmd) -> bool {
    Command::new("tmux")
        .args(cmd.argv())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A v4 uuid from /dev/urandom (no uuid crate). The only randomness in the app.
fn mint_uuid_v4() -> std::io::Result<SessionId> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(SessionId(format_uuid_v4(bytes)))
}

/// Sets the version (4) and RFC 4122 variant bits, then formats 8-4-4-4-12 lowercase hex.
fn format_uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// `--popup`, `--state <path>`, `--dump`, `--help`.
fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<Args> {
    let mut out = Args::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--popup" => out.popup = true,
            "--dump" => out.dump = true,
            "-h" | "--help" => out.help = true,
            "--state" => out.state = Some(args.next().context("--state needs a path")?.into()),
            other => match other.strip_prefix("--state=") {
                Some(p) => out.state = Some(p.into()),
                None => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
            },
        }
    }
    Ok(out)
}

/// A thread blocking on `event::read()`, forwarding each event as `Msg::Term`.
fn spawn_input(tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(Msg::Term(ev)).is_err() {
                break;
            }
        }
    });
}

/// One recursive watch per root (`World::watch_roots` plus the state file's dir); the callback
/// only sends `Msg::Doorbell`. Returned so the loop can re-watch when the roots change.
fn spawn_doorbell(
    tx: mpsc::Sender<Msg>,
    roots: Vec<PathBuf>,
) -> notify::Result<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(Msg::Doorbell);
        }
    })?;
    for root in &roots {
        watcher.watch(root, notify::RecursiveMode::Recursive)?;
    }
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> PaneId {
        PaneId::parse("%251").unwrap()
    }

    fn dir() -> ConfigDir {
        ConfigDir("/Users/u/.claude-personal".into())
    }

    // Defect: jumping with the stale session/window part, or an existence check via
    // `display -p` (which falls back to the current pane, gotcha 31).
    #[test]
    fn jump_commands_target_the_bare_pane_id() {
        assert_eq!(TmuxCmd::ListPane(pane()).argv(), ["list-panes", "-t", "%251"]);
        assert_eq!(TmuxCmd::SwitchClient(pane()).argv(), ["switch-client", "-t", "%251"]);
    }

    // Defect: attach launched without CLAUDE_CONFIG_DIR (lands in the wrong config dir, gotcha
    // 32), or a title with spaces split into several arguments.
    #[test]
    fn attach_window_passes_config_dir_and_keeps_the_title_whole() {
        let cmd = TmuxCmd::NewWindow {
            name: "docs sweep".into(),
            cwd: None,
            config_dir: dir(),
            run: ClaudeRun::Attach(JobId("9b3323e1".into())),
        };
        assert_eq!(
            cmd.argv(),
            [
                "new-window",
                "-n",
                "docs sweep",
                "-e",
                "CLAUDE_CONFIG_DIR=/Users/u/.claude-personal",
                "claude",
                "attach",
                "9b3323e1"
            ]
        );
    }

    // Defect: a typed name with spaces split across argv (claude would see stray positional
    // prompt words), `--name` sent when none was typed, or the cwd dropped.
    #[test]
    fn new_session_window_runs_claude_with_session_id_and_optional_name() {
        let new = |name: Option<&str>| TmuxCmd::NewWindow {
            name: name.unwrap_or("claude").into(),
            cwd: Some("/w/my repo".into()),
            config_dir: dir(),
            run: ClaudeRun::New {
                session: SessionId("0b6c2f7e-1c1a-4d2b-9a8e-3f4a5b6c7d8e".into()),
                name: name.map(str::to_owned),
            },
        };
        assert_eq!(
            new(Some("fix the bug")).argv(),
            [
                "new-window",
                "-n",
                "fix the bug",
                "-c",
                "/w/my repo",
                "-e",
                "CLAUDE_CONFIG_DIR=/Users/u/.claude-personal",
                "claude",
                "--session-id",
                "0b6c2f7e-1c1a-4d2b-9a8e-3f4a5b6c7d8e",
                "--name",
                "fix the bug"
            ]
        );
        assert_eq!(
            new(None).argv(),
            [
                "new-window",
                "-n",
                "claude",
                "-c",
                "/w/my repo",
                "-e",
                "CLAUDE_CONFIG_DIR=/Users/u/.claude-personal",
                "claude",
                "--session-id",
                "0b6c2f7e-1c1a-4d2b-9a8e-3f4a5b6c7d8e"
            ]
        );
    }

    // Defect: a uuid without the v4 version nibble or RFC 4122 variant bits, which
    // `claude --session-id` may reject.
    #[test]
    fn uuid_v4_sets_version_and_variant_bits() {
        assert_eq!(format_uuid_v4([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(format_uuid_v4([0; 16]), "00000000-0000-4000-8000-000000000000");
        let a = mint_uuid_v4().unwrap().0;
        assert_eq!(a.len(), 36);
        assert_ne!(a, mint_uuid_v4().unwrap().0);
    }

    // Defect: a flag silently ignored (e.g. `--popup` typo running the persistent mode), or
    // `--state` swallowing the next flag.
    #[test]
    fn parse_args_accepts_known_flags_and_rejects_the_rest() {
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        assert_eq!(
            parse(&["--popup", "--state", "/tmp/s.toml", "--dump"]).unwrap(),
            Args { popup: true, dump: true, help: false, state: Some("/tmp/s.toml".into()) }
        );
        assert_eq!(parse(&["--state=/x"]).unwrap().state, Some("/x".into()));
        assert!(parse(&["--pop"]).is_err());
        assert!(parse(&["--state"]).is_err());
    }
}
