mod config;
mod derive;
mod jobs;
mod limits;
mod model;
mod registry;
mod sidecars;
mod skills;
mod subagents;
mod transcript;
mod ui;
mod user_state;
mod world;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use notify::Watcher;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::crossterm::{execute, terminal};

use config::Config;
use model::{
    ConfigDir, DiagSource, Diagnostic, Effect, JobId, Outcome, PaneId, Problem, Row, SafeText,
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
    skills: bool,
    help: bool,
    state: Option<PathBuf>,
    scope: ScopeArg,
}

#[derive(Debug, Default, PartialEq)]
enum ScopeArg {
    /// The workspace whose root holds the current directory, else all.
    #[default]
    Here,
    All,
    Named(String),
}

const TICK: Duration = Duration::from_secs(1);
const MIN_REFRESH: Duration = Duration::from_millis(250);
/// How long a pause waits, at most, for the input thread to stop reading the terminal.
const INPUT_POLL: Duration = Duration::from_millis(50);
const DUMP_MAX_REFRESHES: usize = 20;

const DUMP_WIDTH: u16 = 160;

const USAGE: &str =
    "usage: agentty [--popup] [--workspace <name> | --all] [--state <path>] [--dump] [--skills]

  --popup             quit after a successful jump (for tmux display-popup -E)
  --workspace <name>  show this workspace from ~/.config/agentty/config.toml
                      (default: the workspace holding the current directory)
  --all               show every session
  --state <path>      user state file (default ~/.config/agentty/state.toml)
  --dump              print one frame as plain text and exit (no TTY needed)
  --skills            with --dump, print the skills view instead
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
    let config_path = Config::default_path(&roots.home);
    let config = Config::load(&config_path, &roots.home)?;
    let cwd = std::env::current_dir()?;
    let current = match args.scope {
        ScopeArg::Here => config.workspace_at(&cwd),
        ScopeArg::All => None,
        ScopeArg::Named(name) => Some(config.workspace_named(&name).with_context(|| {
            format!("no workspace named {name:?} in {}", config_path.display())
        })?),
    };
    let mut world = World::new(roots, Box::new(RealHost), ReadBudget::default());
    let mut store = user_state::Store::load(state_path)?;
    let accounts: Vec<ConfigDir> = config.accounts.iter().map(|a| a.dir.clone()).collect();
    let mut app = ui::App::new(args.popup).scoped(config, current, cwd);
    if args.skills {
        app = app.with_skills(skills::scan(&accounts));
    }

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
        world.observed_limits(),
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
}

impl TmuxCmd {
    pub fn argv(&self) -> Vec<String> {
        match self {
            TmuxCmd::ListPane(p) => vec!["list-panes".into(), "-t".into(), p.as_str().into()],
            TmuxCmd::SwitchClient(p) => {
                vec!["switch-client".into(), "-t".into(), p.as_str().into()]
            }
        }
    }
}

/// `claude --bg` picks its own session id (it ignores `--session-id`), so the job id it prints
/// is the only handle on the new session.
fn claude_bg(dir: &ConfigDir, cwd: &Path, name: Option<&str>, prompt: Option<&str>) -> Command {
    let mut cmd = Command::new("claude");
    cmd.arg("--bg")
        .args(name.map(|n| ["--name", n]).into_iter().flatten())
        .args(prompt.map(|p| ["--", p]).into_iter().flatten())
        .current_dir(cwd)
        .env("CLAUDE_CONFIG_DIR", &dir.0)
        .stdin(Stdio::null());
    cmd
}

/// Reads the id out of `backgrounded · <id> · <name> (...)`.
fn job_from_bg_output(stdout: &str) -> Option<JobId> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("backgrounded · ")?;
        JobId::parse(rest.split(' ').next()?)
    })
}

fn bg_failure(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or("claude --bg printed no job id".into(), |l| {
            SafeText::redact(l).as_str().to_owned()
        })
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
        Effect::NewSession {
            dir,
            cwd,
            group,
            name,
            prompt,
        } => match claude_bg(&dir, &cwd, name.as_deref(), prompt.as_deref()).output() {
            Err(e) => Outcome::Failed(format!("could not run claude --bg: {e}")),
            Ok(out) => match job_from_bg_output(&String::from_utf8_lossy(&out.stdout)) {
                Some(job) => Outcome::Spawned { dir, job, group },
                None => Outcome::Failed(bg_failure(&String::from_utf8_lossy(&out.stderr))),
            },
        },
        Effect::ScanSkills(accounts) => Outcome::Skills(skills::scan(&accounts)),
        Effect::Attach { dir, job } => match screen.hand_over(&mut claude_attach(&dir, &job)) {
            Ok(status) if status.success() => Outcome::Attached,
            Ok(status) => Outcome::Failed(format!("claude attach failed ({status})")),
            Err(e) => Outcome::Failed(format!("could not run claude attach: {e}")),
        },
        Effect::Jump(_) if std::env::var_os("TMUX").is_none() => failed("not inside tmux"),
        Effect::Jump(pane) => {
            if !tmux(&TmuxCmd::ListPane(pane.clone())) {
                Outcome::Failed(format!("pane {} is gone", pane.as_str()))
            } else if !tmux(&TmuxCmd::SwitchClient(pane)) {
                failed("tmux switch-client failed")
            } else {
                Outcome::Jumped
            }
        }
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

fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<Args> {
    let mut out = Args::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--popup" => out.popup = true,
            "--dump" => out.dump = true,
            "--skills" => out.skills = true,
            "-h" | "--help" => out.help = true,
            "--all" => out.scope = ScopeArg::All,
            "--workspace" => {
                out.scope = ScopeArg::Named(args.next().context("--workspace needs a name")?);
            }
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
    fn new_session_runs_claude_bg_in_the_row_cwd_with_an_optional_name() {
        let cmd = claude_bg(&dir(), Path::new("/w/my repo"), Some("fix the bug"), None);
        assert_eq!(cmd.get_program(), "claude");
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["--bg", "--name", "fix the bug"]
        );
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/w/my repo")));
        assert_eq!(
            cmd.get_envs().collect::<Vec<_>>(),
            [(
                "CLAUDE_CONFIG_DIR".as_ref(),
                Some("/Users/u/.claude-personal".as_ref())
            )]
        );
        let unnamed = claude_bg(&dir(), Path::new("/w"), None, None);
        assert_eq!(unnamed.get_args().collect::<Vec<_>>(), ["--bg"]);
    }

    #[test]
    fn a_prompt_goes_last_after_a_double_dash() {
        let cmd = claude_bg(
            &dir(),
            Path::new("/w"),
            Some("share jot"),
            Some("/share-skill jot"),
        );
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["--bg", "--name", "share jot", "--", "/share-skill jot"]
        );
    }

    #[test]
    fn the_job_id_comes_from_the_backgrounded_line() {
        // Captured from Claude Code 2.1.310.
        let out = "backgrounded · a4bc2210 · agentty bg probe (idle — send a prompt to start)
  claude agents             list sessions
  claude attach a4bc2210    open in this terminal
";
        assert_eq!(job_from_bg_output(out), Some(JobId("a4bc2210".into())));
        assert_eq!(job_from_bg_output("backgrounded · --rm · x"), None);
        assert_eq!(job_from_bg_output(""), None);
    }

    #[test]
    fn a_failed_bg_start_flashes_the_last_stderr_line() {
        let stderr = "warning: something minor
Workspace not trusted. Run `claude` in /w once and accept the trust prompt, then retry.

";
        assert_eq!(
            bg_failure(stderr),
            "Workspace not trusted. Run `claude` in /w once and accept the trust prompt, then retry."
        );
        assert_eq!(bg_failure(""), "claude --bg printed no job id");
    }

    #[test]
    fn parse_args_accepts_known_flags_and_rejects_the_rest() {
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        assert_eq!(
            parse(&["--popup", "--state", "/tmp/s.toml", "--dump"]).unwrap(),
            Args {
                popup: true,
                dump: true,
                skills: false,
                help: false,
                state: Some("/tmp/s.toml".into()),
                scope: ScopeArg::Here,
            }
        );
        assert_eq!(parse(&["--state=/x"]).unwrap().state, Some("/x".into()));
        assert_eq!(parse(&["--all"]).unwrap().scope, ScopeArg::All);
        assert_eq!(
            parse(&["--workspace", "blade"]).unwrap().scope,
            ScopeArg::Named("blade".into())
        );
        assert!(parse(&["--workspace"]).is_err());
        assert!(parse(&["--pop"]).is_err());
        assert!(parse(&["--state"]).is_err());
    }
}
