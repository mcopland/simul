pub mod model;

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::Paragraph;

use crate::client;
use crate::paths::{self, env_var};
use crate::protocol::{Ack, Request, RunState, State, Status, now_ms};
use crate::tmux::Tmux;
use model::{Action, App, Line, Mark, Mode, Tone};

const LOCATION_REFRESH: Duration = Duration::from_secs(5);
/// Lines one wheel notch scrolls.
const SCROLL_STEP: usize = 3;
const RECONNECT: Duration = Duration::from_secs(1);

enum Update {
    Status(Status),
    Disconnected(String),
}

pub fn run() -> Result<()> {
    let socket = paths::client_socket()?;
    let tmux = paths::current_tmux_socket()
        .map(Tmux::new)
        .context("the sidebar must run inside tmux")?;
    let own_pane = env_var("TMUX_PANE");
    let (tx, rx) = mpsc::channel();
    let subscribe_socket = socket.clone();
    std::thread::spawn(move || subscribe_forever(&subscribe_socket, &tx));

    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableFocusChange
    )
    .context("failed to enable mouse capture and focus reporting")?;
    let result = ui_loop(&mut terminal, &rx, &tmux, &socket, own_pane.as_deref());
    // Restoring the terminal is best effort; the pane is usually being closed anyway.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableFocusChange
    );
    ratatui::restore();
    result
}

fn subscribe_forever(socket: &std::path::Path, tx: &mpsc::Sender<Update>) {
    loop {
        let err = match client::subscribe(socket) {
            Ok(stream) => stream
                .map(|item| match item {
                    Ok(status) => tx.send(Update::Status(status)).map_err(|_| None),
                    Err(e) => Err(Some(format!("{e:#}"))),
                })
                .find_map(Result::err)
                .flatten(),
            Err(e) => Some(format!("{e:#}")),
        };
        let Some(err) = err else {
            return; // UI gone.
        };
        if tx.send(Update::Disconnected(err)).is_err() {
            return;
        }
        std::thread::sleep(RECONNECT);
    }
}

struct View {
    app: App,
    error: Option<String>,
    offset: usize,
    /// Rows the list had in the last frame; the footer is the row below them.
    list_height: usize,
    /// What the last frame highlighted, in which view; the list scrolls to the highlight only
    /// when this changes, so neither the wheel nor rows coming and going above it are undone.
    highlighted: Option<(Mode, String)>,
}

impl View {
    fn new(app: App) -> View {
        View {
            app,
            error: None,
            offset: 0,
            list_height: 0,
            highlighted: None,
        }
    }

    /// A left click acts on the line under it, or on the footer switches the view; the wheel
    /// scrolls.
    fn on_mouse(&mut self, m: MouseEvent) -> Action {
        let row = usize::from(m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if row >= self.list_height => {
                // The footer shows an error in place of the view; a click dismisses it.
                if self.error.take().is_none() {
                    self.app.toggle_mode();
                }
                Action::None
            }
            MouseEventKind::Down(MouseButton::Left) => self.app.on_click(self.offset + row),
            MouseEventKind::ScrollDown => {
                self.offset += SCROLL_STEP;
                Action::None
            }
            MouseEventKind::ScrollUp => {
                self.offset = self.offset.saturating_sub(SCROLL_STEP);
                Action::None
            }
            _ => Action::None,
        }
    }
}

fn ui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    rx: &mpsc::Receiver<Update>,
    tmux: &Tmux,
    socket: &Path,
    own_pane: Option<&str>,
) -> Result<()> {
    let mut view = View::new(App {
        home: crate::paths::env_var("HOME").unwrap_or_default(),
        ..App::default()
    });
    let mut location_checked: Option<Instant> = None;
    loop {
        while let Ok(update) = rx.try_recv() {
            match update {
                Update::Status(s) => {
                    view.error = None;
                    view.app.set_status(s);
                }
                Update::Disconnected(e) => view.error = Some(format!("daemon unavailable: {e}")),
            }
        }
        if location_checked.is_none_or(|t| t.elapsed() >= LOCATION_REFRESH) {
            location_checked = Some(Instant::now());
            if let Some(pane) = own_pane {
                match tmux.run(&["display-message", "-p", "-t", pane, "#{window_id}"]) {
                    Ok(out) if !out.trim().is_empty() => {
                        view.app.set_location(out.trim().to_owned());
                    }
                    Ok(_) => view.error = Some(format!("tmux reported no window for {pane}")),
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
            }
        }
        if let Some(status) = view.app.status.as_mut() {
            status.now_ms = now_ms();
        }
        terminal
            .draw(|f| render(f, &mut view))
            .context("failed to draw sidebar")?;

        if !event::poll(Duration::from_millis(250)).context("failed to poll terminal input")? {
            continue;
        }
        let Event::Mouse(m) = event::read().context("failed to read terminal input")? else {
            continue;
        };
        // tmux focuses the pane you click; hand focus straight back to the pane you were in,
        // before a jump, so a jump to this window lands on that pane. Focus that arrives by
        // keyboard stays, so tmux's pane cycling can pass through the sidebar.
        if matches!(m.kind, MouseEventKind::Down(_))
            && let Some(pane) = own_pane
            && let Err(e) = refocus(tmux, pane)
        {
            view.error = Some(format!("{e:#}"));
        }
        let action = view.on_mouse(m);
        let result = match action {
            Action::None => Ok(()),
            Action::Jump(target) => jump(tmux, &target),
            Action::Collapse { session, collapsed } => collapse(socket, session, collapsed),
        };
        if let Err(e) = result {
            view.error = Some(format!("{e:#}"));
        }
    }
}

/// Asks the daemon to fold or unfold `session`; every sidebar sees it in the next status.
fn collapse(socket: &Path, session: String, collapsed: bool) -> Result<()> {
    let ack: Ack = client::request(socket, &Request::Collapse { session, collapsed })?;
    anyhow::ensure!(ack.ok, "the daemon could not set @simul-collapsed");
    Ok(())
}

/// Hands tmux focus back to the pane last used in this sidebar's window, if the sidebar has it.
fn refocus(tmux: &Tmux, own_pane: &str) -> Result<()> {
    let active = tmux.run(&["display-message", "-p", "-t", own_pane, "#{pane_active}"])?;
    if active.trim() == "1" && tmux.run(&["select-pane", "-l", "-t", own_pane]).is_err() {
        // Older tmux has no last pane once it closes; the sidebar is leftmost, so its
        // neighbour is a real pane.
        tmux.run(&["select-pane", "-R", "-t", own_pane])?;
    }
    Ok(())
}

/// `target` is a pane or a window; for a window, its active pane is selected.
fn jump(tmux: &Tmux, target: &str) -> Result<()> {
    tmux.run(&["switch-client", "-t", target])?;
    tmux.run(&["select-window", "-t", target])?;
    tmux.run(&["select-pane", "-t", target])?;
    Ok(())
}

fn state_color(state: State) -> Color {
    match state {
        State::Blocked => Color::Red,
        State::Asking => Color::Cyan,
        State::Done => Color::Green,
        State::Failed => Color::Magenta,
        State::Working => Color::Yellow,
        State::Idle => Color::Blue,
        State::Unknown => Color::DarkGray,
    }
}

fn run_color(run: RunState) -> Color {
    match run {
        RunState::Running => Color::Yellow,
        RunState::Finished => Color::Green,
        RunState::Interactive => Color::Blue,
        RunState::Unknown => Color::DarkGray,
    }
}

fn mark_color(mark: Mark) -> Color {
    match mark {
        Mark::Agent(s) => state_color(s),
        Mark::Run(r) => run_color(r),
    }
}

/// Indents: window headers sit under their session, agents under their window.
const WINDOW_INDENT: usize = 1;
const AGENT_INDENT: usize = 3;
/// Where an agent's text starts after ` <symbol> `; the lines under it indent to match.
const DETAIL_INDENT: usize = AGENT_INDENT + 2;

fn render(f: &mut Frame, view: &mut View) {
    let [list, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let width = usize::from(list.width).saturating_sub(DETAIL_INDENT);
    let lines = view.app.lines();
    let highlighted = view.app.highlighted_line(&lines);
    let tones = view.app.tones();
    let row_style = |line: &Line| match tones.of(line) {
        Tone::Dim => Style::new().fg(Color::DarkGray),
        Tone::Active => Style::new(),
        Tone::Highlighted => Style::new().add_modifier(Modifier::BOLD),
    };
    // Session names take the gold of the session name in tmux's status-left; dim ones darken it.
    let session_style = |line: &Line| {
        let gold = match tones.of(line) {
            Tone::Dim => Color::Rgb(0x93, 0x73, 0x47),
            Tone::Active | Tone::Highlighted => Color::Rgb(0xf6, 0xc1, 0x77),
        };
        row_style(line).fg(gold)
    };
    let rendered: Vec<TextLine> = lines
        .iter()
        .map(|line| match line {
            Line::Session { name, folded, .. } => {
                let style = session_style(line);
                let Some(counts) = folded else {
                    return TextLine::from(Span::styled(
                        model::truncate(name, usize::from(list.width)),
                        style,
                    ));
                };
                let tail: Vec<Span> = counts
                    .iter()
                    .map(|&(mark, n)| {
                        Span::styled(
                            format!(" {}{n}", model::mark_symbol(mark)),
                            Style::new().fg(mark_color(mark)),
                        )
                    })
                    .collect();
                let tail_width: usize = tail.iter().map(|s| s.content.chars().count()).sum();
                // Room for the name before " ▸" and the counts.
                let room = usize::from(list.width).saturating_sub(tail_width + 2);
                let mut spans = vec![Span::styled(
                    format!("{} \u{25b8}", model::truncate(name, room)),
                    style,
                )];
                spans.extend(tail);
                TextLine::from(spans)
            }
            Line::Window { label, .. } => TextLine::from(Span::styled(
                format!(
                    "{:WINDOW_INDENT$}{}",
                    "",
                    model::truncate(label, usize::from(list.width).saturating_sub(WINDOW_INDENT))
                ),
                row_style(line),
            )),
            Line::Agent(row) => TextLine::from(vec![
                Span::styled(
                    format!("{:AGENT_INDENT$}{} ", "", model::symbol(row.state)),
                    Style::new().fg(state_color(row.state)),
                ),
                Span::styled(model::agent_text(row, width), row_style(line)),
            ]),
            Line::Place { text, .. } => TextLine::from(Span::styled(
                format!("{:DETAIL_INDENT$}{}", "", model::truncate(text, width)),
                row_style(line),
            )),
            Line::Pane {
                place, run, age, ..
            } => {
                let style = row_style(line);
                let Some(run) = run else {
                    return TextLine::from(Span::styled(
                        format!("{:DETAIL_INDENT$}{}", "", model::truncate(place, width)),
                        style,
                    ));
                };
                TextLine::from(vec![
                    Span::styled(
                        format!("{:AGENT_INDENT$}{} ", "", model::run_symbol(*run)),
                        Style::new().fg(run_color(*run)),
                    ),
                    Span::styled(model::text_with_age(place, age, width), style),
                ])
            }
        })
        .collect();
    let height = usize::from(list.height);
    view.list_height = height;
    let key = highlighted.map(|i| (view.app.mode, lines[i].target().to_owned()));
    if key != view.highlighted {
        view.highlighted = key;
        if let Some(line) = highlighted {
            view.offset = model::scroll(&lines, line, view.offset, height);
        }
    }
    view.offset = view.offset.min(rendered.len().saturating_sub(height));
    let body = if rendered.is_empty() {
        vec![TextLine::from(Span::styled(
            "no agents",
            Style::new().fg(Color::DarkGray),
        ))]
    } else {
        rendered.into_iter().skip(view.offset).collect()
    };
    f.render_widget(Paragraph::new(body), list);

    let footer_line = match &view.error {
        Some(err) => TextLine::from(Span::styled(err.clone(), Style::new().fg(Color::Red))),
        None => TextLine::from(Span::styled(
            format!("view: {}", view.app.mode.label()),
            Style::new().fg(Color::DarkGray),
        )),
    };
    f.render_widget(Paragraph::new(footer_line), footer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PaneSummary, Run, RunState, WindowInfo};
    use crossterm::event::{KeyModifiers, MouseEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn plain_window(session: &str, pane: &str) -> WindowInfo {
        WindowInfo {
            id: format!("@{session}"),
            session: session.into(),
            index: 1,
            name: "w".into(),
            panes: vec![PaneSummary {
                id: pane.into(),
                command: "zsh".into(),
                active: true,
                cwd: "/".into(),
                git: None,
                run: None,
            }],
        }
    }

    /// Draws a sidebar listing sessions `a` and `b` with item `selected` chosen.
    fn draw() -> Buffer {
        draw_in(None)
    }

    /// Sessions `a` and `b`, each one window with one idle shell, `%1` and `%2`.
    fn two_sessions() -> Status {
        Status {
            now_ms: 0,
            sidebar: true,
            panes: vec![],
            windows: vec![plain_window("a", "%1"), plain_window("b", "%2")],
            collapsed: vec![],
        }
    }

    /// Like `draw`, for a sidebar living in window `own_window`.
    fn draw_in(own_window: Option<&str>) -> Buffer {
        draw_status(two_sessions(), own_window)
    }

    fn view_of(status: Status, own_window: Option<&str>) -> View {
        View::new(App {
            status: Some(status),
            own_window: own_window.map(Into::into),
            ..App::default()
        })
    }

    /// Draws `view` in a 20 by `height` terminal.
    fn frame(view: &mut View, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(20, height)).unwrap();
        terminal.draw(|f| render(f, view)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn draw_status(status: Status, own_window: Option<&str>) -> Buffer {
        frame(&mut view_of(status, own_window), 8)
    }

    const GOLD: Color = Color::Rgb(0xf6, 0xc1, 0x77);

    /// The first drawn cell of row `y`.
    fn first_cell(buf: &Buffer, y: u16) -> &ratatui::buffer::Cell {
        (0..buf.area.width)
            .map(|x| &buf[(x, y)])
            .find(|c| c.symbol() != " ")
            .unwrap()
    }

    #[test]
    fn highlighted_pane_its_window_and_session_are_bold() {
        // Lines: a, 1:w, pane, b, 1:w, pane.
        let buf = draw_in(Some("@a"));
        for row in [0, 1, 2] {
            let cell = first_cell(&buf, row);
            assert!(cell.modifier.contains(Modifier::BOLD), "row {row} bold");
            assert!(!cell.modifier.contains(Modifier::REVERSED), "no block");
        }
        assert_eq!(buf[(0, 0)].fg, GOLD);
        for row in [3, 4, 5] {
            let cell = first_cell(&buf, row);
            assert!(
                !cell.modifier.contains(Modifier::BOLD),
                "row {row} not bold"
            );
        }
    }

    #[test]
    fn unhighlighted_sessions_are_dim() {
        let buf = draw();
        for row in [0, 3] {
            let cell = &buf[(0, row)];
            assert_ne!(cell.fg, GOLD, "row {row}");
            assert!(!cell.modifier.contains(Modifier::BOLD), "row {row}");
        }
    }

    #[test]
    fn own_session_is_bright_gold_and_bold_while_unselected() {
        let buf = draw_in(Some("@b"));
        let (a, b) = (&buf[(0, 0)], &buf[(0, 3)]);
        assert_eq!((a.symbol(), b.symbol()), ("a", "b"));
        assert_eq!(b.fg, GOLD, "own session bright, as the own window is bold");
        assert!(b.modifier.contains(Modifier::BOLD));
        assert_ne!(a.fg, GOLD, "other session is dimmed");
        assert!(!a.modifier.contains(Modifier::BOLD));
    }

    const DIM_GOLD: Color = Color::Rgb(0x93, 0x73, 0x47);

    fn agent(pane: &str, state: State, session: &str) -> crate::protocol::PaneRecord {
        serde_json::from_value(serde_json::json!({
            "pane": pane, "agent": "claude", "state": state, "since_ms": 0,
            "last_event_ms": 0, "session_id": null, "cwd": "/", "branch": null,
            "pending_tool": null, "session": session, "window": 1,
            "window_id": format!("@{session}"), "window_name": "w", "active": false,
        }))
        .unwrap()
    }

    #[test]
    fn an_idle_agent_lights_its_window_and_session_without_bold() {
        let mut s = two_sessions();
        s.panes.push(agent("%1", State::Idle, "a"));
        let buf = draw_status(s, None);
        // Lines: a, 1:w, the agent, b, 1:w, the shell.
        assert_eq!(buf[(0, 0)].fg, GOLD);
        for row in [0, 1, 2] {
            assert!(
                !first_cell(&buf, row).modifier.contains(Modifier::BOLD),
                "row {row}"
            );
        }
        assert_eq!(buf[(1, 1)].fg, Color::Reset, "window");
        assert_eq!(buf[(5, 2)].fg, Color::Reset, "agent");
        assert_eq!(buf[(0, 3)].fg, DIM_GOLD, "nothing runs in b");
    }

    #[test]
    fn session_and_window_with_work_are_normal_the_idle_ones_dim() {
        let mut s = two_sessions();
        s.panes.push(agent("%1", State::Working, "a"));
        let buf = draw_status(s, None);
        // Lines: a, 1:w, the agent, b, 1:w, the shell.
        assert_eq!(buf[(0, 0)].fg, GOLD, "session a holds work");
        assert!(!buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(1, 1)].fg, Color::Reset, "window with work");
        assert!(!buf[(1, 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(5, 2)].fg, Color::Reset, "working agent");
        assert_eq!(buf[(0, 3)].fg, DIM_GOLD);
        assert_eq!(
            buf[(1, 4)].fg,
            Color::DarkGray,
            "nothing runs in b's window"
        );
    }

    #[test]
    fn a_running_process_lights_its_pane_window_and_session() {
        let mut s = two_sessions();
        s.windows[1].panes[0].run = Some(Run {
            state: RunState::Running,
            since_ms: 0,
        });
        let buf = draw_status(s, None);
        assert_eq!(buf[(0, 3)].fg, GOLD, "session b");
        assert_eq!(buf[(1, 4)].fg, Color::Reset, "its window");
        assert_eq!(buf[(5, 5)].fg, Color::Reset, "the pane");
        assert_eq!(buf[(1, 1)].fg, Color::DarkGray, "a's window runs nothing");
        assert_eq!(buf[(5, 2)].fg, Color::DarkGray, "a's shell at its prompt");
    }

    #[test]
    fn idle_shells_are_dim_even_as_their_windows_active_pane_but_bold_when_focused() {
        let buf = draw_status(two_sessions(), Some("@b"));
        // Lines: a, 1:w, %1, b, 1:w, %2; %1 is its window's active pane.
        assert_eq!(
            buf[(5, 2)].fg,
            Color::DarkGray,
            "active pane, nothing running"
        );
        assert!(!buf[(5, 2)].modifier.contains(Modifier::BOLD));
        for row in [3, 4, 5] {
            let cell = first_cell(&buf, row);
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "row {row} highlighted"
            );
            assert_ne!(cell.fg, Color::DarkGray, "row {row} not dimmed");
        }
    }

    /// Twelve sessions, `s0` to `s11`, each one window with one idle shell: 36 lines.
    fn many_sessions() -> Status {
        Status {
            windows: (0..12)
                .map(|i| plain_window(&format!("s{i:02}"), &format!("%{i}")))
                .collect(),
            ..two_sessions()
        }
    }

    fn mouse(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn left(row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), row)
    }

    #[test]
    fn a_click_acts_on_the_scrolled_line_under_it() {
        let mut view = view_of(two_sessions(), None);
        frame(&mut view, 8);
        assert_eq!(view.on_mouse(left(1)), Action::Jump("@a".into()));
        view.offset = 3;
        assert_eq!(view.on_mouse(left(1)), Action::Jump("@b".into()));
        assert_eq!(
            view.on_mouse(left(0)),
            Action::Collapse {
                session: "b".into(),
                collapsed: true
            }
        );
    }

    #[test]
    fn a_click_on_the_footer_switches_the_view() {
        let mut view = view_of(two_sessions(), None);
        let buf = frame(&mut view, 8);
        assert_eq!(row_text(&buf, 7).trim_end(), "view: all");
        assert_eq!(view.on_mouse(left(7)), Action::None);
        assert_eq!(view.app.mode, Mode::Status);
        let buf = frame(&mut view, 8);
        assert_eq!(row_text(&buf, 7).trim_end(), "view: status");
        view.on_mouse(left(7));
        assert_eq!(view.app.mode, Mode::All);
    }

    #[test]
    fn the_wheel_scrolls_within_the_list() {
        let mut view = view_of(many_sessions(), None);
        frame(&mut view, 11);
        view.on_mouse(mouse(MouseEventKind::ScrollDown, 0));
        assert_eq!(view.offset, SCROLL_STEP);
        for _ in 0..30 {
            view.on_mouse(mouse(MouseEventKind::ScrollDown, 0));
        }
        frame(&mut view, 11);
        assert_eq!(
            view.offset,
            36 - 10,
            "stops with the last line at the bottom"
        );
        view.offset = 1;
        view.on_mouse(mouse(MouseEventKind::ScrollUp, 0));
        assert_eq!(view.offset, 0);
    }

    #[test]
    fn the_list_scrolls_to_the_highlight_only_when_it_moves() {
        let mut view = view_of(many_sessions(), Some("@s11"));
        frame(&mut view, 11);
        assert_eq!(view.offset, 36 - 10, "the highlighted last pane in view");
        view.on_mouse(mouse(MouseEventKind::ScrollUp, 0));
        frame(&mut view, 11);
        assert_eq!(view.offset, 36 - 10 - SCROLL_STEP, "the wheel isn't undone");
        view.app.set_location("@s00".into());
        frame(&mut view, 11);
        assert_eq!(view.offset, 0, "follows the highlight to the top");
    }

    #[test]
    fn rows_coming_and_going_above_the_highlight_keep_the_wheel_scroll() {
        let mut view = view_of(many_sessions(), Some("@s11"));
        frame(&mut view, 11);
        view.on_mouse(mouse(MouseEventKind::ScrollUp, 0));
        let scrolled = view.offset;
        let mut s = many_sessions();
        let extra = PaneSummary {
            id: "%99".into(),
            active: false,
            ..s.windows[0].panes[0].clone()
        };
        s.windows[0].panes.push(extra);
        view.app.set_status(s);
        frame(&mut view, 11);
        assert_eq!(view.offset, scrolled, "the highlight is the same pane");
        view.app.toggle_mode();
        view.app.set_location("@s00".into());
        frame(&mut view, 11);
        assert_eq!(view.offset, 0, "a new highlight scrolls");
    }

    #[test]
    fn a_click_on_an_error_dismisses_it_without_switching_the_view() {
        let mut view = view_of(two_sessions(), None);
        view.error = Some("daemon unavailable: gone".into());
        frame(&mut view, 8);
        assert_eq!(view.on_mouse(left(7)), Action::None);
        assert_eq!(view.error, None);
        assert_eq!(view.app.mode, Mode::All);
    }

    /// Draws one window of session `a` whose pane %1 (cwd `/`) has `run`, at time 5s.
    fn draw_run(run: Option<Run>) -> Buffer {
        let mut window = plain_window("a", "%1");
        window.panes[0].run = run;
        let status = Status {
            now_ms: 5_000,
            sidebar: true,
            panes: vec![],
            windows: vec![window],
            collapsed: vec![],
        };
        frame(&mut view_of(status, None), 4)
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn running_pane_shows_its_symbol_like_an_agent_row() {
        let buf = draw_run(Some(Run {
            state: RunState::Running,
            since_ms: 3_000,
        }));
        // Lines: a, 1:w, the pane.
        assert_eq!(buf[(3, 2)].symbol(), "\u{25b6}");
        assert_eq!(buf[(3, 2)].fg, Color::Yellow);
        assert_eq!(
            buf[(5, 2)].symbol(),
            "/",
            "the place starts where an agent's does"
        );
        assert!(
            row_text(&buf, 2).trim_end().ends_with("2s"),
            "{:?}",
            row_text(&buf, 2)
        );
    }

    #[test]
    fn finished_and_interactive_panes_have_their_own_colors() {
        let finished = draw_run(Some(Run {
            state: RunState::Finished,
            since_ms: 0,
        }));
        assert_eq!(finished[(3, 2)].symbol(), "\u{25a0}");
        assert_eq!(finished[(3, 2)].fg, Color::Green);
        let editor = draw_run(Some(Run {
            state: RunState::Interactive,
            since_ms: 0,
        }));
        assert_eq!(editor[(3, 2)].symbol(), "\u{25b7}");
        assert_eq!(editor[(3, 2)].fg, Color::Blue);
        assert_eq!(row_text(&editor, 2).trim_end(), "   \u{25b7} /", "no age");
    }

    #[test]
    fn idle_pane_keeps_its_plain_layout() {
        let buf = draw_run(None);
        assert_eq!(row_text(&buf, 2).trim_end(), "     /");
    }
}
