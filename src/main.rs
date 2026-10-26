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
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use notify::Watcher;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::crossterm::{execute, terminal};

use model::{
    ConfigDir, DiagSource, Diagnostic, Effect, JobId, Outcome, PaneId, Problem, Row, SessionId,
    Snapshot,
};
use world::{ReadBudget, RealHost, Roots, Wake, World};

enum Msg {
    Term(Event),
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
const MIN_REFRESH: Duration = Duration::from_millis(250);
/// How long a pause waits, at most, for the input thread to stop reading the terminal.
const INPUT_POLL: Duration = Duration::from_millis(50);
const DUMP_MAX_REFRESHES: usize = 20;

const DUMP_WIDTH: u16 = 160;

const USAGE: &str = "usage: agentty [--popup] [--state <path>] [--dump]

  --popup         quit after a successful jump (for tmux display-popup -E)
  --state <path>  user state file (default ~/.config/agentty/state.toml)
  --dump          print one frame as plain text and exit (no TTY needed)
";

fn main() -> anyhow::Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    if args.help {
        print!("{USAGE}");
        return Ok(());
    }
    let roots = Roots::from_env()?;
    let state_path = args
        .state
        .unwrap_or_else(|| user_state::Store::default_path(&roots.home));
    let mut world = World::new(roots, Box::new(RealHost), ReadBudget::default());
    let mut store = user_state::Store::load(state_path)?;
    let mut app = ui::App::new(args.popup);

    if args.dump {
        let mut snap = Snapshot::default();
        for _ in 0..DUMP_MAX_REFRESHES {
            world.refresh(Wake::Tick);
            snap = snapshot(&world);
            if !snap
                .diagnostics
                .iter()
                .any(|d| d.problem == Problem::Deferred)
            {
                break;
            }
        }
        print!(
            "{}",
            ui::dump(&app, &snap, store.state(), world.now(), DUMP_WIDTH)
        );
        return Ok(());
    }

    let (tx, rx) = mpsc::channel();
    let input = Arc::new(InputGate::default());
    spawn_input(tx.clone(), input.clone());
    let mut terminal = ratatui::init();
    let mut screen = Screen {
        terminal: &mut terminal,
        input: &input,
    };
    let result = run(&mut screen, tx, rx, &mut world, &mut store, &mut app);
    ratatui::restore();
    result
}

fn snapshot(world: &World) -> Snapshot {
    let now = world.now();
    derive::snapshot(
        world.sessions(),
        world.exited(now).into_iter(),
        world.diagnostics(),
        now,
    )
}

fn run(
    screen: &mut Screen,
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
        let deadline = if doorbell {
            next_tick.min(last_refresh + MIN_REFRESH)
        } else {
            next_tick
        };
        let first = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(msg) => Some(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        for msg in first.into_iter().chain(rx.try_iter()) {
            match msg {
                Msg::Doorbell => doorbell = true,
                Msg::Term(Event::Key(k))
                    if k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    return Ok(());
                }
                Msg::Term(Event::Key(k)) => {
                    if let Some(key) = ui::key_of(&k)
                        && dispatch(app, &snap, store, screen, ui::Input::Key(key), world.now())
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
            let ops =
                user_state::rebind(store.state(), snap.rows.iter().filter_map(Row::live_anchor));
            if ops.iter().any(|op| store.commit(op).is_err()) {
                problems.push(Problem::Unavailable);
            }
            snap.diagnostics
                .extend(problems.into_iter().map(|problem| Diagnostic {
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
                let deferred = snap
                    .diagnostics
                    .iter()
                    .any(|d| d.problem == Problem::Deferred);
                next_tick = if deferred { now } else { now + TICK };
            }
        }
        screen
            .terminal
            .draw(|f| ui::render(f, app, &snap, store.state(), world.now()))?;
    }
}

fn dispatch(
    app: &mut ui::App,
    snap: &Snapshot,
    store: &mut user_state::Store,
    screen: &mut Screen,
    input: ui::Input,
    now: model::At,
) -> bool {
    let mut queue: VecDeque<Effect> = ui::update(app, snap, store.state(), input, now).into();
    while let Some(effect) = queue.pop_front() {
        match execute(effect, store, screen) {
            None => return true,
            Some(outcome) => {
                queue.extend(ui::update(
                    app,
                    snap,
                    store.state(),
                    ui::Input::Done(outcome),
                    now,
                ));
            }
        }
    }
    false
}

fn existing_watch_roots(world: &World, state_file: &Path) -> Vec<PathBuf> {
    world
        .watch_roots()
        .into_iter()
        .chain(state_file.parent().map(Path::to_path_buf))
        .filter(|p| p.exists())
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxCmd {
    /// Existence check: `list-panes -t %N` exits 1 when the pane is gone; `display -p -t %N`
    /// exits 0.
    ListPane(PaneId),
    SwitchClient(PaneId),
    NewWindow {
        cwd: PathBuf,
        config_dir: ConfigDir,
        session: SessionId,
        name: Option<String>,
    },
}

impl TmuxCmd {
    pub fn argv(&self) -> Vec<String> {
        match self {
            TmuxCmd::ListPane(p) => vec!["list-panes".into(), "-t".into(), p.as_str().into()],
            TmuxCmd::SwitchClient(p) => {
                vec!["switch-client".into(), "-t".into(), p.as_str().into()]
            }
            // tmux execs a multi-argument command without `sh -c`, so no element needs quoting.
            TmuxCmd::NewWindow {
                cwd,
                config_dir,
                session,
                name,
            } => {
                let mut v = vec![
                    "new-window".into(),
                    "-n".into(),
                    name.as_deref().unwrap_or("claude").into(),
                    "-c".into(),
                    cwd.display().to_string(),
                    "-e".into(),
                    format!("CLAUDE_CONFIG_DIR={}", config_dir.0.display()),
                    "claude".into(),
                    "--session-id".into(),
                    session.0.clone(),
                ];
                if let Some(name) = name {
                    v.extend(["--name".into(), name.clone()]);
                }
                v
            }
        }
    }
}

fn claude_attach(dir: &ConfigDir, job: &JobId) -> Command {
    let mut cmd = Command::new("claude");
    cmd.args(["attach", &job.0])
        .env("CLAUDE_CONFIG_DIR", &dir.0);
    cmd
}

/// The terminal, and the gate that keeps the input thread off it while a child owns it.
struct Screen<'a> {
    terminal: &'a mut ratatui::DefaultTerminal,
    input: &'a InputGate,
}

impl Screen<'_> {
    /// Runs `cmd` in the foreground on this terminal, then takes the terminal back and repaints.
    fn hand_over(&mut self, cmd: &mut Command) -> std::io::Result<ExitStatus> {
        self.input.pause();
        ratatui::restore();
        let status = cmd.status();
        let back = terminal::enable_raw_mode()
            .and_then(|()| execute!(std::io::stdout(), terminal::EnterAlternateScreen))
            .and_then(|()| self.terminal.clear());
        self.input.resume();
        back.and(status)
    }
}

#[derive(Default)]
struct InputGate {
    state: Mutex<Gate>,
    changed: Condvar,
}

#[derive(Default)]
struct Gate {
    paused: bool,
    /// The input thread is not reading the terminal: it is waiting on `paused`, or it has exited.
    parked: bool,
}

impl InputGate {
    /// Returns once the input thread has stopped reading, so the next keystroke reaches the child.
    fn pause(&self) {
        let mut g = self.state.lock().unwrap();
        g.paused = true;
        while !g.parked {
            g = self.changed.wait(g).unwrap();
        }
    }

    fn resume(&self) {
        self.state.lock().unwrap().paused = false;
        self.changed.notify_all();
    }

    /// Called by the input thread between reads.
    fn wait_while_paused(&self) {
        let mut g = self.state.lock().unwrap();
        if !g.paused {
            return;
        }
        g.parked = true;
        self.changed.notify_all();
        while g.paused {
            g = self.changed.wait(g).unwrap();
        }
        g.parked = false;
    }

    fn close(&self) {
        self.state.lock().unwrap().parked = true;
        self.changed.notify_all();
    }
}

fn execute(effect: Effect, store: &mut user_state::Store, screen: &mut Screen) -> Option<Outcome> {
    let failed = |what: &str| Outcome::Failed(what.to_owned());
    Some(match effect {
        Effect::Quit => return None,
        Effect::Commit(op) => match store.commit(&op) {
            Ok(()) => Outcome::Committed,
            Err(e) => Outcome::Failed(format!("could not save state: {e}")),
        },
        Effect::Attach { dir, job } => match screen.hand_over(&mut claude_attach(&dir, &job)) {
            Ok(status) if status.success() => Outcome::Attached,
            Ok(status) => Outcome::Failed(format!("claude attach failed ({status})")),
            Err(e) => Outcome::Failed(format!("could not run claude attach: {e}")),
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
        Effect::NewSession {
            dir,
            cwd,
            group,
            name,
        } => match mint_uuid_v4() {
            Err(e) => Outcome::Failed(format!("could not mint a session id: {e}")),
            Ok(session) => {
                let cmd = TmuxCmd::NewWindow {
                    cwd,
                    config_dir: dir,
                    session: session.clone(),
                    name,
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
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

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

fn spawn_input(tx: mpsc::Sender<Msg>, gate: Arc<InputGate>) {
    std::thread::spawn(move || {
        loop {
            gate.wait_while_paused();
            match event::poll(INPUT_POLL) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(_) => break,
            }
            let Ok(ev) = event::read() else { break };
            if tx.send(Msg::Term(ev)).is_err() {
                break;
            }
        }
        gate.close();
    });
}

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

    #[test]
    fn jump_commands_target_the_bare_pane_id() {
        assert_eq!(
            TmuxCmd::ListPane(pane()).argv(),
            ["list-panes", "-t", "%251"]
        );
        assert_eq!(
            TmuxCmd::SwitchClient(pane()).argv(),
            ["switch-client", "-t", "%251"]
        );
    }

    #[test]
    fn attach_runs_claude_attach_under_the_job_config_dir() {
        let cmd = claude_attach(&dir(), &JobId("0a1b2c3d".into()));
        assert_eq!(cmd.get_program(), "claude");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["attach", "0a1b2c3d"]);
        assert_eq!(
            cmd.get_envs().collect::<Vec<_>>(),
            [(
                "CLAUDE_CONFIG_DIR".as_ref(),
                Some("/Users/u/.claude-personal".as_ref())
            )]
        );
    }

    #[test]
    fn new_session_window_runs_claude_with_session_id_and_optional_name() {
        let new = |name: Option<&str>| TmuxCmd::NewWindow {
            cwd: "/w/my repo".into(),
            config_dir: dir(),
            session: SessionId("0b6c2f7e-1c1a-4d2b-9a8e-3f4a5b6c7d8e".into()),
            name: name.map(str::to_owned),
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

    #[test]
    fn uuid_v4_sets_version_and_variant_bits() {
        assert_eq!(
            format_uuid_v4([0xff; 16]),
            "ffffffff-ffff-4fff-bfff-ffffffffffff"
        );
        assert_eq!(
            format_uuid_v4([0; 16]),
            "00000000-0000-4000-8000-000000000000"
        );
        let a = mint_uuid_v4().unwrap().0;
        assert_eq!(a.len(), 36);
        assert_ne!(a, mint_uuid_v4().unwrap().0);
    }

    #[test]
    fn parse_args_accepts_known_flags_and_rejects_the_rest() {
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        assert_eq!(
            parse(&["--popup", "--state", "/tmp/s.toml", "--dump"]).unwrap(),
            Args {
                popup: true,
                dump: true,
                help: false,
                state: Some("/tmp/s.toml".into())
            }
        );
        assert_eq!(parse(&["--state=/x"]).unwrap().state, Some("/x".into()));
        assert!(parse(&["--pop"]).is_err());
        assert!(parse(&["--state"]).is_err());
    }
}
