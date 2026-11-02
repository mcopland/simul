pub mod model;

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
use crate::daemon::state::State;
use crate::paths::{self, env_var};
use crate::protocol::{Status, now_ms};
use crate::tmux::Tmux;
use model::{Action, App, Line};

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
    std::thread::spawn(move || subscribe_forever(&socket, &tx));

    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableFocusChange
    )
    .context("failed to enable mouse capture and focus reporting")?;
    let result = ui_loop(&mut terminal, &rx, &tmux, own_pane.as_deref());
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
        if let Action::Jump(target) = action
            && let Err(e) = jump(tmux, &target)
        {
            view.error = Some(format!("{e:#}"));
        }
    }
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
    let current = Style::new().add_modifier(Modifier::BOLD);
    // Matches the session name in tmux's status-left.
    let session = Style::new().fg(Color::Rgb(0xf6, 0xc1, 0x77));
    let muted = Style::new().fg(Color::DarkGray);
    let detail = |pane: &str| {
        if selected_pane == Some(pane) {
            Style::new()
        } else {
            muted
        }
    };
    let rendered: Vec<TextLine> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| match line {
            Line::Session { name } => TextLine::from(Span::styled(
                model::truncate(name, usize::from(list.width)),
                session,
            )),
            // Like tmux's current window, the sidebar's own window stays bold.
            Line::Window { id, label } => TextLine::from(Span::styled(
                format!(
                    "{:WINDOW_INDENT$}{}",
                    "",
                    model::truncate(label, usize::from(list.width).saturating_sub(WINDOW_INDENT))
                ),
                if selected_line == Some(i) || view.app.own_window.as_deref() == Some(id) {
                    current
                } else {
                    muted
                },
            )),
            Line::Agent(row) => TextLine::from(vec![
                Span::styled(
                    format!("{:AGENT_INDENT$}{} ", "", model::symbol(row.state)),
                    Style::new().fg(state_color(row.state)),
                ),
                Span::styled(
                    model::agent_text(row, width),
                    if selected_line == Some(i) {
                        current
                    } else {
                        muted
                    },
                ),
            ]),
            Line::Place { pane, text } => TextLine::from(Span::styled(
                format!("{:DETAIL_INDENT$}{}", "", model::truncate(text, width)),
                detail(pane),
            )),
            Line::Pane { place, .. } => TextLine::from(Span::styled(
                format!("{:DETAIL_INDENT$}{}", "", model::truncate(place, width)),
                if selected_line == Some(i) {
                    current
                } else {
                    muted
                },
            )),
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
            format!("f:{} /:{text}", view.app.filter.label()),
            Style::new().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(footer_line), footer);
}
