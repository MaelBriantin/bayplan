use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};

use super::action::{Action, Status};
use super::{App, Focus, Modal};
use crate::commands::{BayStatus, Container};

const MOVE: &str = "↑↓←→ move · 1-9 URL · ? keys · q quit";

/// Every key, for the `?` window.
const KEYS: &[(&str, &str)] = &[
    (
        "↑↓ j k",
        "select a bay or a container; scroll the logs when zoomed",
    ),
    ("←→ h l Tab", "go to the bays or their containers"),
    ("1-9", "open the URL of that number"),
    ("p", "open the pull request of the selected branch, with gh"),
    (
        "e",
        "open the selected worktree in $EDITOR, beside the screen in Rex, tmux or with $BAYPLAN_EDIT",
    ),
    ("y", "copy the path of the selected worktree"),
    (
        "i",
        "what the selected bay is made of: groups, branch, worktree",
    ),
    (
        "c",
        "CPU and memory of the main stack, each bay and all of them",
    ),
    (
        "u",
        "bp up the selected bay; docker compose up -d on the main stack",
    ),
    ("U", "the same, building the images"),
    ("d", "bp down the selected bay"),
    ("x", "bp rm the selected bay, once confirmed"),
    ("r", "restart the selected container"),
    ("z", "logs on the whole screen, or back"),
    ("PgUp PgDn ^u ^d", "scroll the logs"),
    ("g G", "top of the logs, end of the logs (follows them)"),
    ("Esc", "leave the zoom, then bp watch"),
    ("q", "quit"),
];

pub fn render(frame: &mut Frame, app: &App) {
    if !app.loaded {
        starting(frame, app.ticks);
        return;
    }
    let [main, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    if app.zoom {
        logs(frame, main, app);
    } else {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(30), Constraint::Fill(1)]).areas(main);
        bays(frame, left, app);
        if let Some(bay) = app.selected_bay() {
            details(frame, right, app, bay);
        } else {
            let text = "No bay: create .bayplan/bays/<bay>.toml, then run `bp up <bay>`";
            frame.render_widget(Paragraph::new(text).block(rounded()), right);
        }
    }
    frame.render_widget(Paragraph::new(footer_line(app)), footer);
    match &app.modal {
        Some(Modal::Confirm {
            title,
            question,
            warnings,
            ..
        }) => confirm(frame, title, question, warnings),
        Some(Modal::Running(action)) => running(frame, action),
        Some(Modal::Help) => help(frame),
        Some(Modal::Consumption) => consumption(frame, app),
        Some(Modal::Info) => {
            if let Some(bay) = app.selected_bay() {
                info(frame, bay, app.on_main());
            }
        }
        None => {}
    }
}

/// In place of the keys: the last thing to tell, until the next key, or what keeps the screen from showing
/// the truth, until it is solved.
fn footer_line(app: &App) -> Line<'_> {
    match (&app.message, &app.trouble) {
        (Some(message), _) => Line::from(format!(" {message}")).fg(Color::Yellow),
        (None, Some(trouble)) => Line::from(format!(" {trouble}")).fg(Color::Red),
        (None, None) => Line::from(format!("{} · {MOVE}", actions(app))).dim(),
    }
}

/// The actions on what is selected, first: the other keys are in the `?` window.
fn actions(app: &App) -> &'static str {
    if app.focus == Focus::Containers && app.selected_container().is_some() {
        "r restart"
    } else if app.on_main() {
        "u up · U build"
    } else {
        "u up · d down · x rm"
    }
}

/// Waits for the first read, its dots moving.
fn starting(frame: &mut Frame, ticks: usize) {
    let dots = ".".repeat(ticks / 3 % 4);
    let text = Line::from(vec![
        Span::raw(" Reading the main stack and the bays"),
        Span::raw(format!("{dots:<3}")).fg(Color::Cyan),
    ]);
    let width = u16::try_from(text.width() + 3).unwrap_or(u16::MAX);
    let area = window(frame, width.min(frame.area().width), 3);
    let block = rounded()
        .border_style(Style::new().fg(Color::Cyan))
        .title(" bp watch ");
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn rounded() -> Block<'static> {
    Block::bordered().border_type(BorderType::Rounded)
}

/// Greys the screen out and clears the window drawn over it.
fn window(frame: &mut Frame, width: u16, height: u16) -> Rect {
    let screen = frame.area();
    frame
        .buffer_mut()
        .set_style(screen, Style::new().fg(Color::DarkGray));
    let [row] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(screen);
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(row);
    frame.render_widget(Clear, area);
    area
}

fn rows(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}

fn confirm(frame: &mut Frame, title: &'static str, question: &str, warnings: &[String]) {
    let mut lines = vec![Line::from(question)];
    for warning in warnings {
        lines.push(Line::from(warning.as_str()).fg(Color::Yellow));
    }
    lines.push(Line::default());
    lines.push(Line::from("y confirms · any other key cancels").dim());
    let width = frame.area().width.saturating_sub(4).min(80);
    let area = window(frame, width, rows(lines.len()).saturating_add(2));
    let block = rounded()
        .border_style(Style::new().fg(Color::Red))
        .title(title);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn running(frame: &mut Frame, action: &Action) {
    let screen = frame.area();
    let area = window(
        frame,
        screen.width.saturating_mul(4) / 5,
        screen.height.saturating_mul(4) / 5,
    );
    let (state, color, hint) = match &action.status {
        Status::Running => ("running…".to_owned(), Color::Yellow, " running… "),
        Status::Done => ("done".to_owned(), Color::Green, " q/Esc closes "),
        Status::Failed(reason) => (format!("failed ({reason})"), Color::Red, " q/Esc closes "),
    };
    let block = rounded()
        .border_style(Style::new().fg(color))
        .title(Line::from(vec![
            Span::raw(format!(" {} · ", action.title)),
            Span::raw(state).fg(color),
            Span::raw(" "),
        ]))
        .title_bottom(Line::from(hint).dim().right_aligned());
    let shown = usize::from(area.height.saturating_sub(2));
    let tail = action.lines[action.lines.len().saturating_sub(shown)..].join("\n");
    frame.render_widget(Paragraph::new(tail).block(block), area);
}

fn help(frame: &mut Frame) {
    let width = KEYS
        .iter()
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::raw(format!(" {keys:<width$}  ")).fg(Color::Cyan),
                Span::raw(*what),
            ])
        })
        .collect();
    let area = window(
        frame,
        frame.area().width.saturating_sub(4).min(80),
        rows(lines.len()).saturating_add(2),
    );
    let block = rounded()
        .border_style(Style::new().fg(Color::Cyan))
        .title(" Keys ")
        .title_bottom(Line::from(" any key closes ").dim().right_aligned());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The config of a bay and where it lives, or the main stack's project and tree.
fn info(frame: &mut Frame, bay: &BayStatus, main: bool) {
    let field = |name: &str, value: String| {
        Line::from(vec![
            Span::raw(format!(" {name:<9}")).fg(Color::Cyan),
            Span::raw(value),
        ])
    };
    let mut lines = vec![
        field(
            if main { "Tree" } else { "Worktree" },
            bay.worktree.display().to_string(),
        ),
        field("Project", bay.project.clone()),
        field(
            "Head",
            bay.head.clone().unwrap_or_else(|| "no worktree".to_owned()),
        ),
    ];
    if let Some(error) = &bay.config_error {
        lines.push(Line::from(vec![
            Span::raw(format!(" {:<9}", "Config")).fg(Color::Cyan),
            Span::raw(format!("unreadable: {error}")).fg(Color::Red),
        ]));
    }
    if !main {
        lines.push(field(
            "From",
            bay.from
                .clone()
                .unwrap_or_else(|| "HEAD of the main tree".to_owned()),
        ));
        lines.push(field(
            "Branch",
            bay.branch
                .clone()
                .unwrap_or_else(|| "none: detached HEAD".to_owned()),
        ));
        if bay.groups.is_empty() {
            lines.push(field("Groups", "none".to_owned()));
        }
        for (index, (group, services)) in bay.groups.iter().enumerate() {
            let name = if index == 0 { "Groups" } else { "" };
            lines.push(Line::from(vec![
                Span::raw(format!(" {name:<9}")).fg(Color::Cyan),
                Span::raw(group.as_str()).bold(),
                Span::raw(format!("  {}", services.join(", "))).dim(),
            ]));
        }
    }
    let area = window(
        frame,
        frame.area().width.saturating_sub(4).min(100),
        rows(lines.len()).saturating_add(2),
    );
    let block = rounded()
        .border_style(Style::new().fg(Color::Cyan))
        .title(format!(" {} ", bay.name))
        .title_bottom(Line::from(" any key closes ").dim().right_aligned());
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

/// A bay or the main stack in its list, greyed out when none of its containers runs.
fn item<'a>(name: &'a str, bay: &'a BayStatus) -> ListItem<'a> {
    let running = bay
        .containers
        .iter()
        .any(|container| container.state == "running");
    let name = if running {
        Span::raw(name).bold()
    } else {
        Span::raw(name).dark_gray()
    };
    let item = ListItem::new(vec![
        Line::from(vec![
            name,
            Span::raw("  "),
            Span::raw(bay.summary()).fg(bay_color(bay)),
        ]),
        git_line(bay),
    ]);
    if running { item } else { item.dark_gray() }
}

/// The main stack in a panel of its own above the bays: the selection goes from one to the other.
fn bays(frame: &mut Frame, area: Rect, app: &App) {
    let state = app.selected_bay().map_or(Color::DarkGray, bay_color);
    let focused = app.focus == Focus::Bays;
    let bays_area = if app.has_main {
        let [main_area, bays_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]).areas(area);
        let list = selectable(
            List::new([item(&app.statuses[0].project, &app.statuses[0])]),
            " Main stack ",
            focused && app.on_main(),
            state,
        );
        let mut selected = ListState::default().with_selected(app.on_main().then_some(0));
        frame.render_stateful_widget(list, main_area, &mut selected);
        bays_area
    } else {
        area
    };
    let first = usize::from(app.has_main);
    let items: Vec<ListItem> = app.statuses[first..]
        .iter()
        .map(|bay| item(&bay.name, bay))
        .collect();
    let list = selectable(List::new(items), " Bays ", focused && !app.on_main(), state);
    let index = app
        .selected_bay()
        .filter(|_| !app.on_main())
        .map(|_| app.bay - first);
    let mut selected = ListState::default().with_selected(index);
    frame.render_stateful_widget(list, bays_area, &mut selected);
}

/// The branch of a bay, `●` when its worktree has uncommitted changes, and how far it is from its
/// upstream.
fn git_line(bay: &BayStatus) -> Line<'_> {
    let mut spans = Vec::new();
    if bay.dirty {
        spans.push(Span::raw("● ").fg(Color::Yellow));
    }
    spans.push(Span::raw(bay.head.as_deref().unwrap_or("no worktree")).dim());
    match bay.sync {
        Some((0, 0)) | None => {}
        Some((ahead, behind)) => {
            let ahead = (ahead > 0).then(|| format!("↑{ahead}"));
            let behind = (behind > 0).then(|| format!("↓{behind}"));
            let sync: String = [ahead, behind].into_iter().flatten().collect();
            spans.push(Span::raw(format!(" {sync}")).fg(Color::Magenta));
        }
    }
    Line::from(spans)
}

fn details(frame: &mut Frame, area: Rect, app: &App, bay: &BayStatus) {
    let resource_rows = if bay.resources.is_empty() {
        0
    } else {
        rows(bay.resources.len()).saturating_add(2)
    };
    // The logs keep some room: a long list of containers scrolls, a long list of URLs is cut.
    let [containers, urls, resources, logs_area] = Layout::vertical([
        Constraint::Max(rows(bay.containers.len().max(1)).saturating_add(2)),
        Constraint::Max(rows(bay.urls.len().max(1)).saturating_add(2)),
        Constraint::Length(resource_rows),
        Constraint::Min(8),
    ])
    .areas(area);

    let head = bay.head.as_deref().unwrap_or("no worktree");
    let mut title = if app.on_main() {
        format!(
            " {} · {} · {head} · {}",
            bay.name,
            bay.project,
            bay.summary()
        )
    } else {
        format!(" {} · {head} · {}", bay.name, bay.summary())
    };
    title.push(' ');
    let width = bay
        .containers
        .iter()
        .map(|container| container.service.len())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = bay
        .containers
        .iter()
        .map(|container| ListItem::new(container_line(container, width)))
        .collect();
    let list = if items.is_empty() {
        List::new([ListItem::new(
            Line::from("no container: run `bp up` to start the bay").dim(),
        )])
    } else {
        List::new(items)
    };
    let state = app
        .selected_container()
        .map_or(Color::DarkGray, container_color);
    let list = selectable(list, title, app.focus == Focus::Containers, state);
    let mut state =
        ListState::default().with_selected(app.selected_container().map(|_| app.container));
    frame.render_stateful_widget(list, containers, &mut state);

    let lines: Vec<Line> = if bay.urls.is_empty() {
        vec![Line::from("no URL").dim()]
    } else {
        bay.urls
            .iter()
            .enumerate()
            .map(|(index, url)| {
                Line::from(vec![
                    Span::raw(format!("[{}] ", index + 1)).dim(),
                    Span::raw(url.as_str()).fg(Color::Cyan),
                ])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(rounded().title(" URLs ")), urls);

    if !bay.resources.is_empty() {
        let width = bay
            .resources
            .iter()
            .map(|(name, _)| name.len())
            .max()
            .unwrap_or(0);
        let lines: Vec<Line> = bay
            .resources
            .iter()
            .map(|(name, share)| {
                Line::from(vec![
                    Span::raw(format!("{name:<width$}  ")).dim(),
                    Span::raw(share.as_str()),
                ])
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).block(rounded().title(" Resources ")),
            resources,
        );
    }

    logs(frame, logs_area, app);
}

fn logs(frame: &mut Frame, area: Rect, app: &App) {
    let mut title = vec![Span::raw(" Logs")];
    if let Some(container) = app.selected_container() {
        title.push(Span::raw(format!(" · {}", container.service)));
    }
    if app.view.scroll > 0 {
        title.push(Span::raw(format!(" · ↑{}", app.view.scroll)).fg(Color::Yellow));
    }
    title.push(Span::raw(" "));
    let block = rounded().title(Line::from(title));
    let block = if app.zoom {
        block.title_bottom(Line::from(" z or Esc: back ").dim().right_aligned())
    } else {
        block
    };
    if !app.logs_ready() {
        let waiting = app
            .selected_container()
            .map_or_else(String::new, |container| {
                format!("Loading the logs of {}…", container.service)
            });
        frame.render_widget(Paragraph::new(Line::from(waiting).dim()).block(block), area);
        return;
    }
    let lines: Vec<&str> = app.logs.lines().collect();
    let shown = app
        .view
        .window(lines.len(), usize::from(area.height.saturating_sub(2)));
    let text: Vec<Line> = lines[shown].iter().map(|line| Line::from(*line)).collect();
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn container_line(container: &Container, width: usize) -> Line<'_> {
    let state = match container_color(container) {
        Color::DarkGray => Color::DarkGray,
        _ => match container.state.as_str() {
            "running" => Color::Green,
            "exited" | "dead" => Color::Red,
            _ => Color::Yellow,
        },
    };
    let health = match container.health.as_str() {
        "healthy" => Color::Green,
        "unhealthy" => Color::Red,
        _ => Color::Yellow,
    };
    Line::from(vec![
        Span::raw(format!("{:<width$}  ", container.service)),
        Span::raw(format!("{:<8}", container.state)).fg(state),
        Span::raw(container.health.as_str()).fg(health),
    ])
}

/// CPU percent and memory bytes of these containers together, `None` before Docker measured any of them.
fn usage(containers: &[&Container], app: &App) -> Option<(f64, f64)> {
    let measured: Vec<&(String, String)> = containers
        .iter()
        .filter_map(|container| app.stats.as_ref()?.get(&container.name))
        .collect();
    if measured.is_empty() {
        return None;
    }
    let cpu = measured
        .iter()
        .filter_map(|(cpu, _)| cpu.trim_end_matches('%').parse::<f64>().ok())
        .sum();
    let memory = measured
        .iter()
        .filter_map(|(_, memory)| bytes(memory))
        .sum();
    Some((cpu, memory))
}

/// The CPU and memory of the main stack and of each bay, then of all the bays and of everything.
fn consumption(frame: &mut Frame, app: &App) {
    let bays = &app.statuses[usize::from(app.has_main)..];
    let mut table: Vec<(String, Vec<&Container>, bool)> = app
        .statuses
        .iter()
        .map(|bay| (bay.name.clone(), bay.containers.iter().collect(), false))
        .collect();
    table.push((
        "all bays".to_owned(),
        bays.iter().flat_map(|bay| &bay.containers).collect(),
        true,
    ));
    table.push((
        "everything".to_owned(),
        app.statuses
            .iter()
            .flat_map(|bay| &bay.containers)
            .collect(),
        true,
    ));
    let width = table.iter().map(|(name, ..)| name.len()).max().unwrap_or(0);
    let mut lines = vec![
        Line::from(format!(
            " {:<width$}  {:>10}  {:>7}  {:>9}",
            "", "containers", "CPU", "memory"
        ))
        .dim(),
    ];
    for (index, (name, containers, total)) in table.iter().enumerate() {
        if *total && index > 0 && !table[index - 1].2 {
            lines.push(Line::default());
        }
        let running = containers
            .iter()
            .filter(|container| container.state == "running")
            .count();
        let (cpu, memory) = usage(containers, app).map_or_else(
            || ("-".to_owned(), "-".to_owned()),
            |(cpu, memory)| (format!("{cpu:.1}%"), human(memory)),
        );
        let count = format!("{running}/{}", containers.len());
        let line = Line::from(format!(
            " {name:<width$}  {count:>10}  {cpu:>7}  {memory:>9}"
        ));
        lines.push(if *total { line.bold() } else { line });
    }
    if app.stats.is_none() {
        lines.push(Line::default());
        lines.push(Line::from(" Measuring… Docker takes a couple of seconds").dim());
    }
    let area = window(
        frame,
        u16::try_from(width + 38)
            .unwrap_or(u16::MAX)
            .min(frame.area().width),
        rows(lines.len()).saturating_add(2),
    );
    let block = rounded()
        .border_style(Style::new().fg(Color::Cyan))
        .title(" Consumption ")
        .title_bottom(Line::from(" any key closes ").dim().right_aligned());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Bytes in a size as `docker stats` writes it: `120.4MiB`, `1.2GiB`, `512kB`, `0B`.
fn bytes(size: &str) -> Option<f64> {
    let split = size.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = size.split_at(split);
    let factor = match unit {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(number.trim().parse::<f64>().ok()? * factor)
}

fn human(bytes: f64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024.0 * MIB {
        format!("{:.1}GiB", bytes / (1024.0 * MIB))
    } else {
        format!("{:.0}MiB", bytes / MIB)
    }
}

/// Red for a container failed or unhealthy, yellow for one on its way (starting, restarting), green for
/// one running fine, grey for one that ended without error, as a one-off task does.
fn container_color(container: &Container) -> Color {
    match (container.state.as_str(), container.health.as_str()) {
        ("exited", _) if container.exit_code == 0 => Color::DarkGray,
        ("exited" | "dead", _) | (_, "unhealthy") => Color::Red,
        ("running", "" | "healthy") => Color::Green,
        _ => Color::Yellow,
    }
}

/// The color of the worst container; grey when none runs.
fn worst(containers: &[Container]) -> Color {
    let colors: Vec<Color> = containers.iter().map(container_color).collect();
    if colors.iter().all(|color| *color == Color::DarkGray) {
        Color::DarkGray
    } else if colors.contains(&Color::Red) {
        Color::Red
    } else if colors.contains(&Color::Yellow) {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn bay_color(bay: &BayStatus) -> Color {
    worst(&bay.containers)
}

/// A list marking its selected line with a bar of the color of its state. The list that has the focus has
/// a cyan border and its line in bold; in the other, the bar is dimmed.
fn selectable<'a>(
    list: List<'a>,
    title: impl Into<Line<'a>>,
    focused: bool,
    state: Color,
) -> List<'a> {
    let (border, line, bar) = if focused {
        (
            Style::new().fg(Color::Cyan),
            Style::new().add_modifier(Modifier::BOLD),
            Style::new().fg(state),
        )
    } else {
        (
            Style::new(),
            Style::new(),
            Style::new().fg(state).add_modifier(Modifier::DIM),
        )
    };
    list.block(rounded().border_style(border).title(title))
        .highlight_symbol(Line::styled("▌ ", bar))
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(line)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::tests::{app, status};
    use super::*;

    fn screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_shows(screen: &str, texts: &[&str]) {
        for text in texts {
            assert!(screen.contains(text), "{text} missing from:\n{screen}");
        }
    }

    fn container(state: &str, health: &str) -> Container {
        Container {
            name: "repo-bay-demo-web-1".to_owned(),
            service: "web".to_owned(),
            state: state.to_owned(),
            health: health.to_owned(),
            exit_code: 0,
        }
    }

    #[test]
    fn shows_the_bays_the_selected_one_its_urls_and_the_tail_of_its_logs() {
        let mut app = app(vec![
            status("demo", &["web", "api"], &["https://web-demo.app.localhost"]),
            status("feat", &[], &[]),
        ]);
        let logs: Vec<String> = (1..=40).map(|n| format!("line {n}")).collect();
        app.logs = logs.join("\n");
        app.logs_for = app.selection();
        let screen = screen(&app, 100, 24);
        assert_shows(
            &screen,
            &[
                "demo  up 2/2",
                "feat  down",
                "demo · feat/x@abc1234 · up 2/2",
                "web  running",
                "[1] https://web-demo.app.localhost",
                "Logs · web",
                "line 40",
                "u up · d down · x rm · ↑↓←→ move",
            ],
        );
        assert!(!screen.contains("line 1 "));
    }

    #[test]
    fn shows_the_worktree_state_and_the_resources() {
        let mut bay = status("demo", &["web"], &[]);
        bay.dirty = true;
        bay.sync = Some((2, 1));
        bay.resources = vec![("mysql".to_owned(), "database app_bay_demo".to_owned())];
        let app = app(vec![bay]);
        let screen = screen(&app, 100, 24);
        assert_shows(
            &screen,
            &[
                "● feat/x@abc1234 ↑2↓1",
                "Resources",
                "mysql  database app_bay_demo",
            ],
        );
    }

    #[test]
    fn zooms_on_the_logs_scrolled_back() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        app.logs = (1..=40)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.logs_for = app.selection();
        app.zoom = true;
        app.view.scroll = 10;
        let screen = screen(&app, 100, 24);
        assert_shows(&screen, &["Logs · web · ↑10", "line 30", "z or Esc: back"]);
        assert!(!screen.contains("Bays"));
        assert!(!screen.contains("line 31"));
    }

    #[test]
    fn says_the_logs_are_loading_until_they_come_for_the_selected_container() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        app.logs = "another container's line".to_owned();
        app.zoom = true;
        let screen = screen(&app, 100, 24);
        assert_shows(&screen, &["Loading the logs of web…"]);
        assert!(!screen.contains("another container's line"));
    }

    #[test]
    fn shows_the_main_stack_first_with_its_project() {
        let mut main = status("main", &["db"], &[]);
        main.name = "main stack".to_owned();
        main.project = "shop".to_owned();
        let mut app = app(vec![main, status("demo", &["web"], &[])]);
        app.has_main = true;
        assert_shows(
            &screen(&app, 100, 24),
            &[
                "Main stack",
                "▌ shop  up 1/1",
                "main stack · shop · feat/x@abc1234 · up 1/1",
                "u up · U build · ↑↓←→ move",
            ],
        );
        app.focus = Focus::Containers;
        assert_shows(&screen(&app, 100, 24), &["r restart · ↑↓←→ move"]);
    }

    #[test]
    fn tells_what_to_do_without_a_bay() {
        let starting = super::super::App {
            ticks: 9,
            ..Default::default()
        };
        let screen_text = screen(&starting, 100, 10);
        assert_shows(
            &screen_text,
            &["bp watch", "Reading the main stack and the bays..."],
        );
        assert!(!screen_text.contains("Bays"));
        assert!(
            screen(&app(Vec::new()), 100, 10).contains("No bay: create .bayplan/bays/<bay>.toml")
        );
    }

    #[test]
    fn asks_before_acting_with_what_it_would_cost() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        app.modal = Some(Modal::Confirm {
            title: " Remove ",
            question: "Remove bay demo: its containers, resources and worktree?".to_owned(),
            warnings: vec!["The worktree has uncommitted changes.".to_owned()],
            then: Box::new(super::super::Intent::None),
        });
        assert_shows(
            &screen(&app, 100, 24),
            &[
                "Remove bay demo: its containers, resources and worktree?",
                "The worktree has uncommitted changes.",
                "y confirms · any other key cancels",
            ],
        );
    }

    #[test]
    fn lists_the_keys() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        app.modal = Some(Modal::Help);
        assert_shows(
            &screen(&app, 100, 30),
            &["Keys", "restart the selected container", "any key closes"],
        );
    }

    #[test]
    fn shows_a_command_and_the_tail_of_its_output_over_the_screen() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        let script = "for n in $(seq 1 40); do echo step $n; done; exit 1";
        let mut action = Action::start(
            "bp up demo".to_owned(),
            std::path::Path::new("sh"),
            &["-c", script],
            std::path::Path::new("/"),
        );
        while action.status == Status::Running {
            std::thread::sleep(std::time::Duration::from_millis(10));
            action.poll();
        }
        app.modal = Some(Modal::Running(action));
        let screen = screen(&app, 100, 24);
        assert_shows(
            &screen,
            &[
                "bp up demo · failed (exit status: 1)",
                "step 40",
                "q/Esc closes",
            ],
        );
        assert!(!screen.contains("step 1 "));
    }

    #[test]
    fn colors_a_bay_by_its_worst_container() {
        let mut bay = status("demo", &[], &[]);
        assert_eq!(bay_color(&bay), Color::DarkGray);
        bay.containers = vec![container("running", ""), container("running", "healthy")];
        assert_eq!(bay_color(&bay), Color::Green);
        bay.containers.push(container("restarting", ""));
        assert_eq!(bay_color(&bay), Color::Yellow);
        bay.containers.push(container("running", "unhealthy"));
        assert_eq!(bay_color(&bay), Color::Red);
        assert_eq!(container_color(&container("exited", "")), Color::DarkGray);
        let failed = Container {
            exit_code: 1,
            ..container("exited", "")
        };
        assert_eq!(container_color(&failed), Color::Red);
        bay.containers = vec![container("running", ""), container("exited", "")];
        assert_eq!(bay_color(&bay), Color::Green);
    }

    #[test]
    fn adds_up_the_cpu_and_memory_of_each_bay_and_of_all() {
        let mut main = status("main", &["db"], &[]);
        main.name = "main stack".to_owned();
        let mut app = app(vec![
            main,
            status("demo", &["web", "api"], &[]),
            status("feat", &["web"], &[]),
        ]);
        app.has_main = true;
        for (name, cpu, memory) in [
            ("repo-bay-main-db-1", "10.00%", "1GiB"),
            ("repo-bay-demo-web-1", "1.50%", "512MiB"),
            ("repo-bay-demo-api-1", "0.50%", "1GiB"),
            ("repo-bay-feat-web-1", "2.00%", "100MB"),
        ] {
            app.stats
                .get_or_insert_default()
                .insert(name.to_owned(), (cpu.to_owned(), memory.to_owned()));
        }
        app.modal = Some(Modal::Consumption);
        assert_shows(
            &screen(&app, 120, 24),
            &[
                "Consumption",
                "main stack         1/1    10.0%     1.0GiB",
                "demo               2/2     2.0%     1.5GiB",
                "all bays           3/3     4.0%     1.6GiB",
                "everything         4/4    14.0%     2.6GiB",
            ],
        );
        assert_eq!(bytes("120.4MiB"), Some(120.4 * 1024.0 * 1024.0));
        assert_eq!(bytes("512kB"), Some(512_000.0));
        assert_eq!(bytes("?"), None);
    }

    #[test]
    fn shows_what_a_bay_is_made_of() {
        let mut bay = status("demo", &["web"], &[]);
        bay.groups = vec![
            (
                "back".to_owned(),
                vec!["api".to_owned(), "worker".to_owned()],
            ),
            ("front".to_owned(), vec!["web".to_owned()]),
        ];
        bay.branch = Some("feat/x".to_owned());
        let mut app = app(vec![bay]);
        app.modal = Some(Modal::Info);
        assert_shows(
            &screen(&app, 110, 24),
            &[
                "Worktree /repo/.bayplan/worktrees/demo",
                "Project  repo-bay-demo",
                "From     HEAD of the main tree",
                "Branch   feat/x",
                "Groups   back  api, worker",
                "front  web",
            ],
        );
    }

    #[test]
    fn keeps_a_trouble_in_place_of_the_keys_after_a_message() {
        let mut app = app(vec![status("demo", &["web"], &[])]);
        app.trouble = Some("Docker is not reachable".to_owned());
        assert_shows(&screen(&app, 100, 24), &["Docker is not reachable"]);
        app.message = Some("copied /repo".to_owned());
        let screen = screen(&app, 100, 24);
        assert_shows(&screen, &["copied /repo"]);
        assert!(!screen.contains("Docker is not reachable"));
    }

    #[test]
    fn tells_a_bay_config_cannot_be_read() {
        let mut bay = status("demo", &["web"], &[]);
        bay.config_error = Some("invalid demo.toml".to_owned());
        let mut app = app(vec![bay]);
        app.modal = Some(Modal::Info);
        assert_shows(
            &screen(&app, 110, 24),
            &["Config   unreadable: invalid demo.toml"],
        );
    }

    #[test]
    fn greys_out_a_bay_where_nothing_runs() {
        let style_of = |app: &App, text: &str| {
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal.draw(|frame| render(frame, app)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let width = usize::from(buffer.area.width);
            let screen: Vec<String> = buffer
                .content
                .chunks(width)
                .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect())
                .collect();
            let (y, row) = screen
                .iter()
                .enumerate()
                .find(|(_, row)| row.contains(text))
                .unwrap();
            let x = row[..row.find(text).unwrap()].chars().count();
            buffer[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())].fg
        };
        let app = app(vec![
            status("demo", &["web"], &[]),
            status("feat", &[], &[]),
        ]);
        assert_eq!(style_of(&app, "feat  down"), Color::DarkGray);
        assert_ne!(style_of(&app, "demo  up"), Color::DarkGray);
    }
}
