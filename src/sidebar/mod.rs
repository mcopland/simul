pub mod model;

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::Paragraph;

use crate::client;
use crate::paths::{self, env_var};
use crate::protocol::{Ack, Request, RunState, State, Status, now_ms};
use crate::tmux::Tmux;
use model::{Action, App, Line, Lit, Mark};

const LOCATION_REFRESH: Duration = Duration::from_secs(5);
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
}

fn ui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    rx: &mpsc::Receiver<Update>,
    tmux: &Tmux,
    socket: &Path,
    own_pane: Option<&str>,
) -> Result<()> {
    let mut view = View {
        app: App {
            home: crate::paths::env_var("HOME").unwrap_or_default(),
            ..App::default()
        },
        error: None,
        offset: 0,
    };
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
        let action = match event::read().context("failed to read terminal input")? {
            Event::Key(k) if k.kind == KeyEventKind::Press => view.app.on_key(k),
            Event::FocusGained => {
                view.app.on_focus(true);
                Action::None
            }
            Event::FocusLost => {
                view.app.on_focus(false);
                Action::None
            }
            Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                view.app.on_click(view.offset + usize::from(m.row))
            }
            _ => Action::None,
        };
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
    let selected_line = view.app.selected.and_then(|n| model::line_of(&lines, n));
    let selected_pane = selected_line.and_then(|i| match &lines[i] {
        Line::Agent(r) => Some(r.pane.as_str()),
        _ => None,
    });
    let own_session = view.app.own_session();
    let lit = view.app.status.as_ref().map(Lit::of).unwrap_or_default();
    let current = Style::new().add_modifier(Modifier::BOLD);
    let muted = Style::new().fg(Color::DarkGray);
    // Bold marks the selected row and the sidebar's own window; of the rest, only what is
    // inactive is dimmed.
    let row_style = |line: &Line, bold: bool| {
        if bold {
            current
        } else if lit.line(line) {
            Style::new()
        } else {
            muted
        }
    };
    // Lit sessions take the gold of the session name in tmux's status-left; the rest dim it.
    let gold = Style::new().fg(Color::Rgb(0xf6, 0xc1, 0x77));
    let session_dim = Style::new().fg(Color::Rgb(0x93, 0x73, 0x47));
    let rendered: Vec<TextLine> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| match line {
            Line::Session { name, folded, .. } => {
                let style = if selected_line == Some(i) || own_session == Some(name.as_str()) {
                    gold.add_modifier(Modifier::BOLD)
                } else if lit.line(line) {
                    gold
                } else {
                    session_dim
                };
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
            // Like tmux's current window, the sidebar's own window stays bold.
            Line::Window { id, label } => TextLine::from(Span::styled(
                format!(
                    "{:WINDOW_INDENT$}{}",
                    "",
                    model::truncate(label, usize::from(list.width).saturating_sub(WINDOW_INDENT))
                ),
                row_style(
                    line,
                    selected_line == Some(i) || view.app.own_window.as_deref() == Some(id),
                ),
            )),
            Line::Agent(row) => TextLine::from(vec![
                Span::styled(
                    format!("{:AGENT_INDENT$}{} ", "", model::symbol(row.state)),
                    Style::new().fg(state_color(row.state)),
                ),
                Span::styled(
                    model::agent_text(row, width),
                    row_style(line, selected_line == Some(i)),
                ),
            ]),
            // The selected agent's place line is lit, not bold.
            Line::Place { pane, text } => TextLine::from(Span::styled(
                format!("{:DETAIL_INDENT$}{}", "", model::truncate(text, width)),
                if selected_pane == Some(pane.as_str()) {
                    Style::new()
                } else {
                    row_style(line, false)
                },
            )),
            Line::Pane {
                place, run, age, ..
            } => {
                let style = row_style(line, selected_line == Some(i));
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
    if let Some(line) = selected_line {
        view.offset = model::scroll(&lines, line, view.offset, height);
    }
    view.offset = view.offset.min(rendered.len().saturating_sub(1));
    let body = if rendered.is_empty() {
        vec![TextLine::from(Span::styled(
            "no agents",
            Style::new().fg(Color::DarkGray),
        ))]
    } else {
        rendered.into_iter().skip(view.offset).collect()
    };
    f.render_widget(Paragraph::new(body), list);

    let footer_line = if let Some(err) = &view.error {
        TextLine::from(Span::styled(err.clone(), Style::new().fg(Color::Red)))
    } else if view.app.editing {
        TextLine::from(format!("/{}_", view.app.text))
    } else {
        let text = if view.app.text.is_empty() {
            "-".to_owned()
        } else {
            view.app.text.clone()
        };
        TextLine::from(Span::styled(
            format!("f:{} /:{text}", view.app.mode.label()),
            Style::new().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(footer_line), footer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PaneSummary, Run, RunState, WindowInfo};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn plain_window(session: &str, pane: &str) -> WindowInfo {
        WindowInfo {
            id: format!("@{session}"),
            session: session.into(),
            index: 1,
            name: "w".into(),
            active: false,
            attached: false,
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
    fn draw(selected: Option<usize>) -> Buffer {
        draw_in(None, selected)
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
    fn draw_in(own_window: Option<&str>, selected: Option<usize>) -> Buffer {
        draw_status(two_sessions(), own_window, selected)
    }

    fn draw_status(status: Status, own_window: Option<&str>, selected: Option<usize>) -> Buffer {
        let mut view = View {
            app: App {
                status: Some(status),
                own_window: own_window.map(Into::into),
                selected,
                ..App::default()
            },
            error: None,
            offset: 0,
        };
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        terminal.draw(|f| render(f, &mut view)).unwrap();
        terminal.backend().buffer().clone()
    }

    const GOLD: Color = Color::Rgb(0xf6, 0xc1, 0x77);

    #[test]
    fn selected_session_is_bright_gold_and_bold_the_rest_dim() {
        // Lines: a, 1:w, pane, b, 1:w, pane; item 0 is session a.
        let buf = draw(Some(0));
        let (a, b) = (&buf[(0, 0)], &buf[(0, 3)]);
        assert_eq!((a.symbol(), b.symbol()), ("a", "b"));
        assert_eq!(a.fg, GOLD);
        assert!(a.modifier.contains(Modifier::BOLD));
        assert!(!a.modifier.contains(Modifier::REVERSED), "no block");
        assert_ne!(b.fg, GOLD, "unselected session is dimmed");
        assert!(
            matches!(b.fg, Color::Rgb(..)),
            "still gold-toned: {:?}",
            b.fg
        );
        assert!(!b.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn unselected_sessions_are_dim() {
        let buf = draw(None);
        for row in [0, 3] {
            let cell = &buf[(0, row)];
            assert_ne!(cell.fg, GOLD, "row {row}");
            assert!(!cell.modifier.contains(Modifier::BOLD), "row {row}");
        }
    }

    #[test]
    fn own_session_is_bright_gold_and_bold_while_unselected() {
        let buf = draw_in(Some("@b"), None);
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
    fn attached_session_is_bright_gold_but_not_bold() {
        let mut s = two_sessions();
        s.windows[0].attached = true;
        let buf = draw_status(s, None, None);
        let (a, b) = (&buf[(0, 0)], &buf[(0, 3)]);
        assert_eq!(a.fg, GOLD);
        assert!(!a.modifier.contains(Modifier::BOLD));
        assert_eq!(b.fg, DIM_GOLD, "detached and idle");
    }

    #[test]
    fn session_and_window_with_work_are_normal_the_idle_ones_dim() {
        let mut s = two_sessions();
        s.panes.push(agent("%1", State::Working, "a"));
        let buf = draw_status(s, None, None);
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
            "idle, not the current window"
        );
    }

    #[test]
    fn current_window_is_normal() {
        let mut s = two_sessions();
        s.windows[1].active = true;
        let buf = draw_status(s, None, None);
        assert_eq!(buf[(1, 1)].fg, Color::DarkGray);
        assert_eq!(buf[(1, 4)].fg, Color::Reset);
        assert!(!buf[(1, 4)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn active_pane_is_normal_inactive_idle_pane_dim_selected_bold() {
        let mut s = two_sessions();
        let mut second = s.windows[0].panes[0].clone();
        second.id = "%3".into();
        second.active = false;
        s.windows[0].panes.push(second);
        // Lines: a, 1:w, %1, %3, b, 1:w, %2; item 6 is %2.
        let buf = draw_status(s, None, Some(6));
        assert_eq!(buf[(5, 2)].fg, Color::Reset, "active pane");
        assert!(!buf[(5, 2)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(5, 3)].fg, Color::DarkGray, "inactive idle pane");
        assert!(buf[(5, 6)].modifier.contains(Modifier::BOLD), "selected");
    }

    /// Draws one window of session `a` whose pane %1 (cwd `/`) has `run`, at time 5s.
    fn draw_run(run: Option<Run>) -> Buffer {
        let mut window = plain_window("a", "%1");
        window.panes[0].run = run;
        let mut view = View {
            app: App {
                status: Some(Status {
                    now_ms: 5_000,
                    sidebar: true,
                    panes: vec![],
                    windows: vec![window],
                    collapsed: vec![],
                }),
                ..App::default()
            },
            error: None,
            offset: 0,
        };
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        terminal.draw(|f| render(f, &mut view)).unwrap();
        terminal.backend().buffer().clone()
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
