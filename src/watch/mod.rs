//! `bp watch`: the bays, their containers, URLs and logs, refreshed as they run.

mod action;
mod logs;
mod ui;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use anyhow::{Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::commands::{self, BayStatus, Container};
use crate::compose::runner;
use crate::config::Layout;
use crate::state::BayState;
use crate::{exec, git};
use action::{Action, Status};
use logs::LogView;

/// Time between two reads of the bays and of the selected container's logs.
const REFRESH: Duration = Duration::from_secs(2);

/// Log lines read for the selected container, to scroll back through.
const LOG_LINES: usize = 1000;

/// Lines a page key scrolls the logs by.
const PAGE: usize = 10;

/// The container whose logs are read: its Compose project and service.
type Selection = Option<(String, String)>;

/// Container name → its CPU and memory use.
type Stats = BTreeMap<String, (String, String)>;

/// What the reader of the bays sends back after each read.
struct Snapshot {
    /// The main stack first when its project can be told, then the bays.
    statuses: Vec<BayStatus>,
    has_main: bool,
    error: Option<String>,
}

/// Logs read for a container.
struct Logs {
    selection: Selection,
    text: String,
}

/// Name of the main stack in the list of bays.
const MAIN: &str = "main stack";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Bays,
    Containers,
}

/// What a key asks the screen to do.
#[derive(Debug, PartialEq, Eq)]
enum Intent {
    None,
    Quit,
    Open(String),
    /// Runs `bp` with these arguments, in a window.
    Run(Vec<String>),
    /// Runs `docker` with these arguments, in a window under this title.
    Docker {
        title: String,
        args: Vec<String>,
    },
    /// Asks before removing the selected bay.
    ConfirmRemoval,
    /// Measures the CPU and memory of the containers, once.
    Measure,
    /// Opens the pull request of the branch checked out in this tree, if it has one.
    PullRequest(PathBuf),
    /// Copies the path of this tree.
    Copy(PathBuf),
    /// Opens this tree in the editor.
    Edit(PathBuf),
    /// Reads the bays again now.
    Refresh,
}

/// A window over the screen.
enum Modal {
    /// Asks before doing `then`, with what it would cost.
    Confirm {
        title: &'static str,
        question: String,
        warnings: Vec<String>,
        then: Box<Intent>,
    },
    Running(Action),
    /// The keys.
    Help,
    /// What the selected bay is made of: its config and where it lives.
    Info,
    /// The CPU and memory of each bay and of all of them.
    Consumption,
}

struct App {
    statuses: Vec<BayStatus>,
    /// Whether the first status is the main stack's.
    has_main: bool,
    /// The CPU and memory measured for `c`, `None` while Docker measures them.
    stats: Option<Stats>,
    logs: String,
    /// The container `logs` were read for.
    logs_for: Selection,
    bay: usize,
    container: usize,
    focus: Focus,
    /// Last thing to tell: an error, or the URL just opened.
    message: Option<String>,
    /// Whether a first read came back.
    loaded: bool,
    /// The bay to select on the first read: the one whose worktree `watch` was started in.
    start_on: Option<String>,
    /// What keeps the screen from showing the truth, as Docker out of reach, until it is solved.
    trouble: Option<String>,
    /// Turns of the screen, to animate what waits.
    ticks: usize,
    modal: Option<Modal>,
    /// Whether the logs fill the screen.
    zoom: bool,
    view: LogView,
}

impl Default for App {
    fn default() -> Self {
        Self {
            statuses: Vec::new(),
            has_main: false,
            stats: Some(Stats::new()),
            logs: String::new(),
            logs_for: None,
            bay: 0,
            container: 0,
            focus: Focus::Bays,
            message: None,
            loaded: false,
            start_on: None,
            trouble: None,
            ticks: 0,
            modal: None,
            zoom: false,
            view: LogView::default(),
        }
    }
}

impl App {
    fn selected_bay(&self) -> Option<&BayStatus> {
        self.statuses.get(self.bay)
    }

    fn on_main(&self) -> bool {
        self.has_main && self.bay == 0
    }

    /// Whether the logs shown are the selected container's, rather than still the previous one's.
    fn logs_ready(&self) -> bool {
        self.logs_for == self.selection()
    }

    fn selected_container(&self) -> Option<&Container> {
        self.selected_bay()?.containers.get(self.container)
    }

    fn selection(&self) -> Selection {
        let bay = self.selected_bay()?;
        let container = self.selected_container()?;
        Some((bay.project.clone(), container.service.clone()))
    }

    /// Takes a new read, keeping the selected bay and container by name when they are still there.
    fn update(&mut self, snapshot: Snapshot) {
        let before = self.selection();
        let bay = self.selected_bay().map(|bay| bay.name.clone());
        let container = self
            .selected_container()
            .map(|container| container.service.clone());
        self.statuses = snapshot.statuses;
        self.has_main = snapshot.has_main;
        self.bay = bay
            .and_then(|name| self.statuses.iter().position(|bay| bay.name == name))
            .unwrap_or(self.bay.min(self.statuses.len().saturating_sub(1)));
        let containers = self
            .selected_bay()
            .map_or(&[][..], |bay| bay.containers.as_slice());
        self.container = container
            .and_then(|service| containers.iter().position(|c| c.service == service))
            .unwrap_or(0);
        // The bays are what is watched: the one `watch` was started in, or else the first one, is selected
        // rather than the main stack.
        if !self.loaded {
            let start = self
                .start_on
                .take()
                .and_then(|name| self.statuses.iter().position(|bay| bay.name == name));
            if let Some(index) = start {
                self.bay = index;
            } else if self.has_main && self.statuses.len() > 1 {
                self.bay = 1;
            }
            self.container = 0;
        }
        if self.selection() != before {
            self.view.reset();
        }
        self.trouble = snapshot.error;
        self.loaded = true;
    }

    fn key(&mut self, key: KeyEvent) -> Intent {
        if key.kind != KeyEventKind::Press {
            return Intent::None;
        }
        if self.modal.is_some() {
            return self.modal_key(key.code);
        }
        self.message = None;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if control => self.scroll_up(PAGE),
            KeyCode::Char('d') if control => self.view.down(PAGE),
            KeyCode::PageUp => self.scroll_up(PAGE),
            KeyCode::PageDown => self.view.down(PAGE),
            KeyCode::Char('G') | KeyCode::End => self.view.scroll = 0,
            KeyCode::Char('g') | KeyCode::Home => self.scroll_up(usize::MAX),
            KeyCode::Up | KeyCode::Char('k') if self.zoom => self.scroll_up(1),
            KeyCode::Down | KeyCode::Char('j') if self.zoom => self.view.down(1),
            KeyCode::Char('z') => self.zoom = !self.zoom,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('c') => {
                self.stats = None;
                self.modal = Some(Modal::Consumption);
                return Intent::Measure;
            }
            KeyCode::Char('i') if self.selected_bay().is_some() => self.modal = Some(Modal::Info),
            KeyCode::Char('y') => {
                if let Some(bay) = self.selected_bay() {
                    return Intent::Copy(bay.worktree.clone());
                }
            }
            KeyCode::Char('e') => {
                if let Some(bay) = self.selected_bay() {
                    return Intent::Edit(bay.worktree.clone());
                }
            }
            KeyCode::Char('p') => {
                if let Some(bay) = self.selected_bay() {
                    let tree = bay.worktree.clone();
                    self.message = Some("looking for the pull request…".to_owned());
                    return Intent::PullRequest(tree);
                }
            }
            KeyCode::Char('q') => return Intent::Quit,
            KeyCode::Esc => {
                if self.zoom {
                    self.zoom = false;
                } else {
                    return Intent::Quit;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Bays => Focus::Containers,
                    Focus::Containers => Focus::Bays,
                };
            }
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Bays,
            KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Containers,
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                let url = self
                    .selected_bay()
                    .and_then(|bay| bay.urls.get(index))
                    .cloned();
                match url {
                    Some(url) => return Intent::Open(url),
                    None => self.message = Some(format!("no URL [{digit}] for this bay")),
                }
            }
            code => return self.command_key(code),
        }
        Intent::None
    }

    fn modal_key(&mut self, code: KeyCode) -> Intent {
        match self.modal.take() {
            Some(Modal::Confirm { then, .. }) => {
                if code == KeyCode::Char('y') {
                    return *then;
                }
            }
            Some(Modal::Running(action)) => {
                let closes = matches!(
                    code,
                    KeyCode::Char(' ' | 'q') | KeyCode::Enter | KeyCode::Esc
                );
                if action.status != Status::Running && closes {
                    return Intent::Refresh;
                }
                self.modal = Some(Modal::Running(action));
            }
            Some(Modal::Help | Modal::Info | Modal::Consumption) | None => {}
        }
        Intent::None
    }

    /// Keys acting on the selected bay or container: `bp` commands on a bay, `docker compose` ones on the
    /// main stack.
    fn command_key(&mut self, code: KeyCode) -> Intent {
        let Some(bay) = self.selected_bay() else {
            return Intent::None;
        };
        let mut compose = vec!["compose".to_owned(), "-p".to_owned(), bay.project.clone()];
        if !self.on_main() {
            compose.extend([
                "-f".to_owned(),
                bay.compose_file.to_string_lossy().into_owned(),
            ]);
        }
        let docker = |command: &[&str]| Intent::Docker {
            title: format!("docker compose {}", command.join(" ")),
            args: compose
                .iter()
                .cloned()
                .chain(command.iter().map(|arg| (*arg).to_owned()))
                .collect(),
        };
        if let (KeyCode::Char('r'), Some(container)) = (code, self.selected_container()) {
            return docker(&["restart", &container.service]);
        }
        let name = bay.name.clone();
        match (code, self.on_main()) {
            (KeyCode::Char('u'), false) => Intent::Run(vec!["up".to_owned(), name]),
            (KeyCode::Char('U'), false) => {
                Intent::Run(vec!["up".to_owned(), name, "--build".to_owned()])
            }
            (KeyCode::Char('d'), false) => Intent::Run(vec!["down".to_owned(), name]),
            (KeyCode::Char('x'), false) => Intent::ConfirmRemoval,
            (KeyCode::Char('u'), true) => docker(&["up", "-d"]),
            (KeyCode::Char('U'), true) => docker(&["up", "-d", "--build"]),
            (KeyCode::Char('d'), true) => {
                self.message = Some(
                    "the bays depend on the main stack: stop it with docker compose, not from here"
                        .to_owned(),
                );
                Intent::None
            }
            (KeyCode::Char('x'), true) => {
                self.message = Some("the main stack is not a bay: it cannot be removed".to_owned());
                Intent::None
            }
            _ => Intent::None,
        }
    }

    fn scroll_up(&mut self, lines: usize) {
        let total = if self.logs_ready() {
            self.logs.lines().count()
        } else {
            0
        };
        self.view.up(lines, total);
    }

    fn step(&mut self, down: bool) {
        let (index, len) = match self.focus {
            Focus::Bays => (&mut self.bay, self.statuses.len()),
            Focus::Containers => (
                &mut self.container,
                self.statuses
                    .get(self.bay)
                    .map_or(0, |bay| bay.containers.len()),
            ),
        };
        let moved = if down {
            (*index + 1).min(len.saturating_sub(1))
        } else {
            index.saturating_sub(1)
        };
        if moved != *index {
            *index = moved;
            if self.focus == Focus::Bays {
                self.container = 0;
            }
            self.view.reset();
        }
    }
}

/// The ends of the channels the screen talks to its readers through.
struct Readers {
    snapshots: Receiver<Snapshot>,
    logs: Receiver<Logs>,
    usage: Receiver<Stats>,
    /// Where a measure of the CPU and memory sends its result.
    measures: Sender<Stats>,
    /// What a task run in the background has to tell.
    notes: Receiver<String>,
    note_sender: Sender<String>,
    /// Asks for a read of the bays now.
    refresh: Sender<()>,
    /// Tells the reader of the logs which container is selected, or to read again.
    selections: Sender<Selection>,
}

/// The bay whose worktree holds `dir`, from the worktrees of the bays.
fn bay_of(dir: &Path, worktrees: &[(String, PathBuf)]) -> Option<String> {
    worktrees
        .iter()
        .find(|(_, worktree)| dir.starts_with(worktree))
        .map(|(bay, _)| bay.clone())
}

pub fn run() -> Result<()> {
    let layout = Layout::discover()?;
    let root = layout.root.clone();
    let worktrees: Vec<(String, PathBuf)> = layout
        .bays()?
        .into_iter()
        .filter_map(|bay| {
            let worktree = layout.worktree(&bay).canonicalize().ok()?;
            Some((bay, worktree))
        })
        .collect();
    let start_on = std::env::current_dir()?
        .canonicalize()
        .ok()
        .and_then(|dir| bay_of(&dir, &worktrees));
    let (selections, selection_reader) = mpsc::channel();
    let (refresh, refresh_reader) = mpsc::channel();
    let (snapshot_sender, snapshots) = mpsc::channel();
    let (logs_sender, logs) = mpsc::channel();
    let (measures, usage) = mpsc::channel();
    let (note_sender, notes) = mpsc::channel();
    std::thread::spawn(move || read_loop(&layout, &refresh_reader, &snapshot_sender));
    std::thread::spawn(move || logs_loop(&selection_reader, &logs_sender));
    let readers = Readers {
        snapshots,
        logs,
        usage,
        measures,
        notes,
        note_sender,
        refresh,
        selections,
    };
    ratatui::run(|terminal| event_loop(terminal, &root, &readers, start_on))
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    root: &Path,
    readers: &Readers,
    start_on: Option<String>,
) -> Result<()> {
    let bp: PathBuf = std::env::current_exe()?;
    let mut app = App {
        start_on,
        ..App::default()
    };
    // `None` until a first selection is sent, and again to ask for a read now.
    let mut sent: Option<Selection> = None;
    loop {
        app.ticks = app.ticks.wrapping_add(1);
        while let Ok(snapshot) = readers.snapshots.try_recv() {
            app.update(snapshot);
        }
        if let Some(logs) = readers.logs.try_iter().last() {
            app.logs = logs.text;
            app.logs_for = logs.selection;
        }
        if let Some(note) = readers.notes.try_iter().last() {
            app.message = Some(note);
        }
        if let Some(latest) = readers.usage.try_iter().last() {
            app.stats = Some(latest);
        }
        if let Some(Modal::Running(action)) = &mut app.modal {
            action.poll();
        }
        let selection = app.selection();
        if sent.as_ref() != Some(&selection) {
            // The readers end only with this loop: a failed send cannot happen before.
            let _ = readers.selections.send(selection.clone());
            sent = Some(selection);
        }
        terminal.draw(|frame| ui::render(frame, &app))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        match app.key(key) {
            Intent::Quit => return Ok(()),
            Intent::Open(url) => {
                app.message = Some(match open(&url) {
                    Ok(()) => format!("opened {url}"),
                    Err(error) => format!("cannot open {url}: {error:#}"),
                });
            }
            Intent::Run(args) => {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let title = format!("bp {}", args.join(" "));
                app.modal = Some(Modal::Running(Action::start(title, &bp, &args, root)));
            }
            Intent::Docker { title, args } => {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let docker = Path::new("docker");
                app.modal = Some(Modal::Running(Action::start(title, docker, &args, root)));
            }
            Intent::ConfirmRemoval => confirm_removal(&mut app),
            Intent::Measure => {
                let measures = readers.measures.clone();
                // A failed measure shows none; the screen closed meanwhile takes no result.
                std::thread::spawn(move || {
                    let _ = measures.send(runner::stats().unwrap_or_default());
                });
            }
            Intent::Copy(tree) => {
                let path = tree.display().to_string();
                app.message = Some(match copy(&path) {
                    Ok(()) => format!("copied {path}"),
                    Err(error) => format!("cannot copy the path: {error:#}"),
                });
            }
            Intent::Edit(tree) => app.message = edit(terminal, &tree, &readers.note_sender),
            Intent::PullRequest(tree) => {
                let notes = readers.note_sender.clone();
                std::thread::spawn(move || {
                    let _ = notes.send(open_pull_request(&tree));
                });
            }
            Intent::Refresh => {
                let _ = readers.refresh.send(());
                sent = None;
            }
            Intent::None => {}
        }
    }
}

fn confirm_removal(app: &mut App) {
    let Some(bay) = app.selected_bay() else {
        return;
    };
    let risks = if bay.worktree.exists() {
        commands::removal_risks(&bay.worktree)
    } else {
        Ok(Vec::new())
    };
    match risks {
        Ok(risks) => {
            app.modal = Some(Modal::Confirm {
                title: " Remove ",
                question: format!(
                    "Remove bay {}: its containers, resources and worktree?",
                    bay.name
                ),
                warnings: risks
                    .iter()
                    .map(|risk| format!("The worktree has {risk}."))
                    .collect(),
                then: Box::new(Intent::Run(vec![
                    "rm".to_owned(),
                    bay.name.clone(),
                    "--force".to_owned(),
                ])),
            });
        }
        Err(error) => app.message = Some(format!("cannot check the worktree: {error:#}")),
    }
}

/// Reads the main stack and the bays, again after each refresh delay or when asked, until the screen is
/// closed.
fn read_loop(layout: &Layout, refresh: &Receiver<()>, snapshots: &Sender<Snapshot>) {
    let mut main = None;
    loop {
        // Told again until Compose answers, as Docker may come up after the screen.
        if main.is_none() {
            main = runner::main_project_name(&layout.root).ok();
        }
        if snapshots.send(read(layout, main.as_deref())).is_err() {
            return;
        }
        match refresh.recv_timeout(REFRESH) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn read(layout: &Layout, main: Option<&str>) -> Snapshot {
    let (bays, mut error) = match commands::statuses(layout) {
        Ok(statuses) => (statuses, None),
        Err(error) => (Vec::new(), Some(format!("cannot read the bays: {error:#}"))),
    };
    let docker = exec::output(
        Path::new("."),
        "docker",
        ["info", "--format", "{{.ServerVersion}}"],
    );
    if docker.is_err() {
        error = Some(
            "Docker is not reachable: the main stack and the bays show as down until it is"
                .to_owned(),
        );
    }
    let mut statuses = Vec::new();
    if let Some(project) = main {
        statuses.push(main_status(layout, project));
    }
    let has_main = !statuses.is_empty();
    statuses.extend(bays);
    Snapshot {
        statuses,
        has_main,
        error,
    }
}

/// The main stack as an entry of the list: its containers, the main tree's branch, and the hosts the bays
/// stand in for.
fn main_status(layout: &Layout, project: &str) -> BayStatus {
    let root = &layout.root;
    let urls = layout
        .bays()
        .unwrap_or_default()
        .iter()
        .filter_map(|bay| BayState::load(&layout.bay_dir(bay).join("state.json")).ok())
        .flat_map(|state| state.hosts.into_keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|host| format!("https://{host}"))
        .collect();
    BayStatus {
        name: MAIN.to_owned(),
        project: project.to_owned(),
        compose_file: PathBuf::new(),
        worktree: root.clone(),
        head: Some(commands::head(root)),
        dirty: git::is_dirty(root).unwrap_or(false),
        sync: git::ahead_behind(root),
        urls,
        containers: commands::containers(project),
        resources: Vec::new(),
        groups: Vec::new(),
        from: None,
        branch: None,
        config_error: None,
    }
}

/// Reads the logs of the selected container as soon as it is selected, then again after each refresh
/// delay, until the screen is closed.
fn logs_loop(selections: &Receiver<Selection>, logs: &Sender<Logs>) {
    let mut selection = None;
    loop {
        match selections.recv_timeout(REFRESH) {
            Ok(newer) => selection = selections.try_iter().last().unwrap_or(newer),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let text = selection
            .as_ref()
            .map_or_else(String::new, |(project, service)| {
                runner::logs(project, service, LOG_LINES)
                    .unwrap_or_else(|error| format!("cannot read the logs: {error:#}"))
            });
        let read = Logs {
            selection: selection.clone(),
            text,
        };
        if logs.send(read).is_err() {
            return;
        }
    }
}

/// Finds the pull request of the branch checked out in `tree` with the GitHub CLI and opens it; returns
/// what happened, to tell.
fn open_pull_request(tree: &Path) -> String {
    if !tree.exists() {
        return "no worktree: no branch, no pull request".to_owned();
    }
    let found = exec::output(tree, "gh", ["pr", "view", "--json", "url", "--jq", ".url"]);
    match found {
        Ok(url) => {
            let url = url.trim();
            match open(url) {
                Ok(()) => format!("opened {url}"),
                Err(error) => format!("cannot open {url}: {error:#}"),
            }
        }
        Err(error) => {
            let error = format!("{error:#}");
            if error.contains("no pull requests found") {
                "no pull request for this branch".to_owned()
            } else if error.contains("cannot start") {
                "the GitHub CLI, gh, is needed to find the pull request".to_owned()
            } else {
                format!("cannot find the pull request: {error}")
            }
        }
    }
}

/// Copies `text` to the clipboard, with the tool of the system.
fn copy(text: &str) -> Result<()> {
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    };
    for (program, args) in tools {
        let Ok(mut child) = Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut input) = child.stdin.take() {
            input.write_all(text.as_bytes())?;
        }
        if child.wait()?.success() {
            return Ok(());
        }
    }
    bail!("no clipboard tool: pbcopy, wl-copy or xclip")
}

/// Where the editor opens: beside the screen when told how, or in its place.
#[derive(Debug, PartialEq, Eq)]
enum EditorPlace {
    /// The command of `$BAYPLAN_EDIT`, where `{dir}` and `{editor}` stand for the tree and the editor.
    Command(String),
    /// A new window of this Rex session.
    Rex(String),
    /// A new tmux window.
    Tmux,
    /// This terminal, the screen handed over to the editor until it quits.
    Here,
}

/// `$BAYPLAN_EDIT` first, then the terminal `watch` runs in, from `$REX_SESSION` and `$TMUX`.
fn editor_place(
    command: Option<String>,
    rex_session: Option<String>,
    tmux: Option<String>,
) -> EditorPlace {
    let set = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
    if let Some(command) = set(command) {
        EditorPlace::Command(command)
    } else if let Some(session) = set(rex_session) {
        EditorPlace::Rex(session)
    } else if set(tmux).is_some() {
        EditorPlace::Tmux
    } else {
        EditorPlace::Here
    }
}

/// `text` between single quotes, for `sh`.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The program and arguments opening `tree` in `editor` beside the screen, `None` to open it here.
fn editor_window(
    place: &EditorPlace,
    editor: &str,
    tree: &Path,
) -> Option<(&'static str, Vec<String>)> {
    let path = tree.display().to_string();
    let name = tree
        .file_name()
        .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned());
    match place {
        EditorPlace::Command(command) => {
            let command = command
                .replace("{dir}", &quote(&path))
                .replace("{editor}", editor);
            Some(("sh", vec!["-c".to_owned(), command]))
        }
        EditorPlace::Rex(session) => {
            let mut args = vec![
                "run".to_owned(),
                "-s".to_owned(),
                session.clone(),
                "-c".to_owned(),
                path.clone(),
                "--label".to_owned(),
                name,
                "--".to_owned(),
            ];
            args.extend(editor.split_whitespace().map(str::to_owned));
            args.push(path);
            Some(("rex", args))
        }
        EditorPlace::Tmux => {
            let window = format!("{editor} {}", quote(&path));
            Some((
                "tmux",
                vec![
                    "new-window".to_owned(),
                    "-c".to_owned(),
                    path,
                    "-n".to_owned(),
                    name,
                    window,
                ],
            ))
        }
        EditorPlace::Here => None,
    }
}

/// Opens `tree` in `$VISUAL` or `$EDITOR`: with `$BAYPLAN_EDIT`, in a new Rex or tmux window, or else in
/// this terminal, the screen handed over to it until it quits; returns what to tell, if anything.
fn edit(terminal: &mut DefaultTerminal, tree: &Path, notes: &Sender<String>) -> Option<String> {
    let editor = ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|variable| std::env::var(variable).ok())
        .find(|editor| !editor.trim().is_empty());
    let Some(editor) = editor else {
        return Some("no editor: set $EDITOR".to_owned());
    };
    if !tree.exists() {
        return Some("no worktree to open".to_owned());
    }
    let place = editor_place(
        std::env::var("BAYPLAN_EDIT").ok(),
        std::env::var("REX_SESSION").ok(),
        std::env::var("TMUX").ok(),
    );
    if let Some((program, args)) = editor_window(&place, &editor, tree) {
        // In the background: a command that does not return at once must not freeze the screen.
        let notes = notes.clone();
        let tree = tree.to_path_buf();
        let opening = format!("opening {editor}…");
        std::thread::spawn(move || {
            let note = match exec::output(&tree, program, &args) {
                Ok(_) => format!("opened {editor} beside bp watch"),
                Err(error) => format!("cannot open {editor}: {error:#}"),
            };
            let _ = notes.send(note);
        });
        return Some(opening);
    }
    ratatui::restore();
    // Through `sh`, as `$EDITOR` may hold arguments, like `code -w`.
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(tree)
        .current_dir(tree)
        .status();
    *terminal = ratatui::init();
    match status {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("{editor} ended with {status}")),
        Err(error) => Some(format!("cannot start {editor}: {error}")),
    }
}

/// Opens a URL in the default browser, quietly: the output of the opener would break the screen.
fn open(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    exec::output(Path::new("."), opener, [url]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn status(name: &str, services: &[&str], urls: &[&str]) -> BayStatus {
        BayStatus {
            name: name.to_owned(),
            project: format!("repo-bay-{name}"),
            compose_file: PathBuf::from(format!("/repo/.bayplan/bays/{name}/compose.json")),
            worktree: PathBuf::from(format!("/repo/.bayplan/worktrees/{name}")),
            head: Some("feat/x@abc1234".to_owned()),
            dirty: false,
            sync: None,
            urls: urls.iter().map(|url| (*url).to_owned()).collect(),
            containers: services
                .iter()
                .map(|service| Container {
                    name: format!("repo-bay-{name}-{service}-1"),
                    service: (*service).to_owned(),
                    state: "running".to_owned(),
                    health: String::new(),
                    exit_code: 0,
                })
                .collect(),
            resources: Vec::new(),
            groups: Vec::new(),
            from: None,
            branch: None,
            config_error: None,
        }
    }

    pub fn app(statuses: Vec<BayStatus>) -> App {
        let mut app = App::default();
        app.update(Snapshot {
            statuses,
            has_main: false,
            error: None,
        });
        app
    }

    /// An app whose first entry is the main stack.
    fn with_main(bays: Vec<BayStatus>) -> App {
        let mut main = status("main", &["db", "redis"], &[]);
        main.name = MAIN.to_owned();
        main.project = "shop".to_owned();
        let mut app = App::default();
        app.update(Snapshot {
            statuses: std::iter::once(main).chain(bays).collect(),
            has_main: true,
            error: None,
        });
        app
    }

    fn press(app: &mut App, code: KeyCode) -> Intent {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn moves_through_the_bays_then_their_containers() {
        let mut app = app(vec![
            status("demo", &["web", "api"], &[]),
            status("feat", &["web"], &[]),
        ]);
        assert_eq!(
            app.selection(),
            Some(("repo-bay-demo".into(), "web".into()))
        );
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(
            app.selection(),
            Some(("repo-bay-demo".into(), "api".into()))
        );
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            app.selection(),
            Some(("repo-bay-feat".into(), "web".into()))
        );
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.bay, 1);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.bay, 0);
    }

    #[test]
    fn goes_to_the_bays_with_h_and_to_the_containers_with_l() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.focus, Focus::Containers);
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.focus, Focus::Bays);
    }

    #[test]
    fn opens_the_url_of_its_number() {
        let mut app = app(vec![status(
            "demo",
            &["web"],
            &[
                "https://web-demo.app.localhost",
                "https://api-demo.app.localhost",
            ],
        )]);
        assert_eq!(
            press(&mut app, KeyCode::Char('2')),
            Intent::Open("https://api-demo.app.localhost".to_owned())
        );
        assert_eq!(press(&mut app, KeyCode::Char('3')), Intent::None);
        assert_eq!(app.message.as_deref(), Some("no URL [3] for this bay"));
        assert_eq!(press(&mut app, KeyCode::Char('q')), Intent::Quit);
    }

    #[test]
    fn keeps_the_selection_by_name_across_reads() {
        let mut app = app(vec![
            status("demo", &["web"], &[]),
            status("feat", &["api", "web"], &[]),
        ]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Down);
        app.update(Snapshot {
            statuses: vec![status("feat", &["worker", "api", "web"], &[])],
            has_main: false,
            error: None,
        });
        assert_eq!(
            app.selection(),
            Some(("repo-bay-feat".into(), "web".into()))
        );

        app.update(Snapshot {
            statuses: Vec::new(),
            has_main: false,
            error: Some("cannot read the bays".to_owned()),
        });
        assert_eq!(app.selection(), None);
        assert_eq!(app.trouble.as_deref(), Some("cannot read the bays"));
    }

    #[test]
    fn runs_up_build_and_down_on_the_selected_bay() {
        let mut app = app(vec![
            status("demo", &["web"], &[]),
            status("feat", &[], &[]),
        ]);
        press(&mut app, KeyCode::Down);
        assert_eq!(
            press(&mut app, KeyCode::Char('u')),
            Intent::Run(vec!["up".into(), "feat".into()])
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('U')),
            Intent::Run(vec!["up".into(), "feat".into(), "--build".into()])
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('d')),
            Intent::Run(vec!["down".into(), "feat".into()])
        );
        assert_eq!(press(&mut App::default(), KeyCode::Char('u')), Intent::None);
    }

    #[test]
    fn restarts_the_selected_container() {
        let mut app = app(vec![status("demo", &["web", "api"], &[])]);
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            press(&mut app, KeyCode::Char('r')),
            Intent::Docker {
                title: "docker compose restart api".to_owned(),
                args: [
                    "compose",
                    "-p",
                    "repo-bay-demo",
                    "-f",
                    "/repo/.bayplan/bays/demo/compose.json",
                    "restart",
                    "api",
                ]
                .map(str::to_owned)
                .to_vec(),
            }
        );
        let mut empty = self::app(vec![status("demo", &[], &[])]);
        assert_eq!(press(&mut empty, KeyCode::Char('r')), Intent::None);
    }

    #[test]
    fn starts_on_the_bay_whose_worktree_holds_the_directory() {
        let worktrees = [
            (
                "demo".to_owned(),
                PathBuf::from("/repo/.bayplan/worktrees/demo"),
            ),
            (
                "feat".to_owned(),
                PathBuf::from("/repo/.bayplan/worktrees/feat"),
            ),
        ];
        let inside = Path::new("/repo/.bayplan/worktrees/feat/api/src");
        assert_eq!(bay_of(inside, &worktrees).as_deref(), Some("feat"));
        assert_eq!(bay_of(Path::new("/repo/api"), &worktrees), None);

        let mut app = App {
            start_on: Some("feat".to_owned()),
            ..App::default()
        };
        let mut main = status("main", &["db"], &[]);
        main.name = MAIN.to_owned();
        app.update(Snapshot {
            statuses: vec![
                main,
                status("demo", &["web"], &[]),
                status("feat", &["api"], &[]),
            ],
            has_main: true,
            error: None,
        });
        assert_eq!(app.selected_bay().unwrap().name, "feat");
    }

    #[test]
    fn starts_on_the_first_bay_rather_than_the_main_stack() {
        assert_eq!(with_main(vec![status("demo", &["web"], &[])]).bay, 1);
        assert!(with_main(Vec::new()).on_main());
    }

    #[test]
    fn runs_docker_compose_on_the_main_stack_but_never_stops_it() {
        let mut app = with_main(vec![status("demo", &["web"], &[])]);
        press(&mut app, KeyCode::Char('k'));
        assert!(app.on_main());
        let compose = |command: &[&str]| {
            ["compose", "-p", "shop"]
                .iter()
                .chain(command)
                .map(|arg| (*arg).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            press(&mut app, KeyCode::Char('U')),
            Intent::Docker {
                title: "docker compose up -d --build".to_owned(),
                args: compose(&["up", "-d", "--build"]),
            }
        );
        press(&mut app, KeyCode::Char('l'));
        let Intent::Docker { args, .. } = press(&mut app, KeyCode::Char('r')) else {
            panic!("no restart");
        };
        assert_eq!(args, compose(&["restart", "db"]));

        assert_eq!(press(&mut app, KeyCode::Char('d')), Intent::None);
        assert!(app.modal.is_none());
        assert!(app.message.as_deref().unwrap().contains("not from here"));

        assert_eq!(press(&mut app, KeyCode::Char('x')), Intent::None);
        assert!(app.modal.is_none());
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('j'));
        assert!(!app.on_main());
        assert_eq!(
            press(&mut app, KeyCode::Char('d')),
            Intent::Run(vec!["down".into(), "demo".into()])
        );
    }

    #[test]
    fn waits_for_the_logs_of_the_selected_container() {
        let mut app = app(vec![status("demo", &["web", "api"], &[])]);
        app.logs = "web log".to_owned();
        app.logs_for = app.selection();
        assert!(app.logs_ready());
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('j'));
        assert!(!app.logs_ready());
    }

    #[test]
    fn removes_a_bay_only_once_confirmed() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        assert_eq!(press(&mut app, KeyCode::Char('x')), Intent::ConfirmRemoval);
        confirm_removal(&mut app);
        let Some(Modal::Confirm { question, .. }) = &app.modal else {
            panic!("no confirmation");
        };
        assert_eq!(
            question,
            "Remove bay demo: its containers, resources and worktree?"
        );
        assert_eq!(press(&mut app, KeyCode::Char('n')), Intent::None);
        assert!(app.modal.is_none());

        confirm_removal(&mut app);
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            Intent::Run(vec!["rm".into(), "demo".into(), "--force".into()])
        );
    }

    #[test]
    fn keeps_a_running_command_open_until_it_ends() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        let running = Action::start(
            "bp up demo".into(),
            Path::new("sh"),
            &["-c", "sleep 1"],
            Path::new("/"),
        );
        app.modal = Some(Modal::Running(running));
        assert_eq!(press(&mut app, KeyCode::Esc), Intent::None);
        assert_eq!(press(&mut app, KeyCode::Char('q')), Intent::None);
        assert!(app.modal.is_some());

        let mut done = Action::start(
            "bp up demo".into(),
            Path::new("sh"),
            &["-c", "true"],
            Path::new("/"),
        );
        while done.status == Status::Running {
            std::thread::sleep(Duration::from_millis(10));
            done.poll();
        }
        app.modal = Some(Modal::Running(done));
        assert_eq!(press(&mut app, KeyCode::Char('q')), Intent::Refresh);
        assert!(app.modal.is_none());
    }

    #[test]
    fn scrolls_and_zooms_the_logs() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        app.logs = (1..=50)
            .map(|n| {
                if n % 20 == 0 {
                    format!("ERROR {n}")
                } else {
                    format!("line {n}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        app.logs_for = app.selection();
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.view.scroll, PAGE);
        app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.view.scroll, 0);

        press(&mut app, KeyCode::Char('z'));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.view.scroll, 1);
        assert_eq!(app.bay, 0);
        press(&mut app, KeyCode::Esc);
        assert!(!app.zoom);

        assert_eq!(press(&mut app, KeyCode::Esc), Intent::Quit);
    }

    #[test]
    fn shows_the_bay_on_i_until_any_key() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        press(&mut app, KeyCode::Char('i'));
        assert!(matches!(app.modal, Some(Modal::Info)));
        press(&mut app, KeyCode::Char('x'));
        assert!(app.modal.is_none());
        press(&mut App::default(), KeyCode::Char('i'));
    }

    #[test]
    fn looks_for_the_pull_request_of_the_selected_tree() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        assert_eq!(
            press(&mut app, KeyCode::Char('p')),
            Intent::PullRequest(PathBuf::from("/repo/.bayplan/worktrees/demo"))
        );
        assert_eq!(
            app.message.as_deref(),
            Some("looking for the pull request…")
        );
        assert_eq!(
            open_pull_request(Path::new("/no/such/tree")),
            "no worktree: no branch, no pull request"
        );
    }

    #[test]
    fn copies_or_edits_the_selected_tree() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        let tree = PathBuf::from("/repo/.bayplan/worktrees/demo");
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            Intent::Copy(tree.clone())
        );
        assert_eq!(press(&mut app, KeyCode::Char('e')), Intent::Edit(tree));
        assert_eq!(press(&mut App::default(), KeyCode::Char('e')), Intent::None);
    }

    #[test]
    fn opens_the_editor_with_the_command_of_the_user_or_in_rex_or_tmux() {
        let rex = "rex run -c {dir} -- {editor} {dir}";
        let tmux = || Some("/tmp/tmux".to_owned());
        assert_eq!(
            editor_place(Some(rex.into()), Some("session:1".into()), tmux()),
            EditorPlace::Command(rex.into())
        );
        assert_eq!(
            editor_place(Some(" ".into()), Some("session:1".into()), tmux()),
            EditorPlace::Rex("session:1".into())
        );
        assert_eq!(editor_place(None, None, tmux()), EditorPlace::Tmux);
        assert_eq!(editor_place(None, None, None), EditorPlace::Here);

        let tree = Path::new("/repo/worktrees/it's");
        let (program, args) =
            editor_window(&EditorPlace::Command(rex.into()), "nvim", tree).unwrap();
        assert_eq!(program, "sh");
        assert_eq!(
            args,
            [
                "-c",
                r"rex run -c '/repo/worktrees/it'\''s' -- nvim '/repo/worktrees/it'\''s'"
            ]
        );
        let (program, args) =
            editor_window(&EditorPlace::Rex("session:1".into()), "code -w", tree).unwrap();
        assert_eq!(program, "rex");
        assert_eq!(
            args,
            [
                "run",
                "-s",
                "session:1",
                "-c",
                "/repo/worktrees/it's",
                "--label",
                "it's",
                "--",
                "code",
                "-w",
                "/repo/worktrees/it's",
            ]
        );
        let (program, args) = editor_window(&EditorPlace::Tmux, "nvim", tree).unwrap();
        assert_eq!(program, "tmux");
        assert_eq!(
            args,
            [
                "new-window",
                "-c",
                "/repo/worktrees/it's",
                "-n",
                "it's",
                r"nvim '/repo/worktrees/it'\''s'"
            ]
        );
        assert_eq!(editor_window(&EditorPlace::Here, "nvim", tree), None);
    }

    #[test]
    fn keeps_a_trouble_until_a_read_says_otherwise() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        app.update(Snapshot {
            statuses: Vec::new(),
            has_main: false,
            error: Some("Docker is not reachable".to_owned()),
        });
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.trouble.as_deref(), Some("Docker is not reachable"));
        app.update(Snapshot {
            statuses: Vec::new(),
            has_main: false,
            error: None,
        });
        assert_eq!(app.trouble, None);
    }

    #[test]
    fn tells_until_the_next_key() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        press(&mut app, KeyCode::Char('9'));
        assert!(app.message.is_some());
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.message, None);
    }

    #[test]
    fn shows_the_consumption_on_c_until_any_key() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        assert_eq!(press(&mut app, KeyCode::Char('c')), Intent::Measure);
        assert!(matches!(app.modal, Some(Modal::Consumption)));
        assert!(app.stats.is_none());
        press(&mut app, KeyCode::Char('c'));
        assert!(app.modal.is_none());
    }

    #[test]
    fn shows_the_keys_until_any_is_pressed() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        press(&mut app, KeyCode::Char('?'));
        assert!(matches!(app.modal, Some(Modal::Help)));
        assert_eq!(press(&mut app, KeyCode::Char('q')), Intent::None);
        assert!(app.modal.is_none());
    }
}
