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

const SESSION_REFRESH: Duration = Duration::from_secs(5);
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
    home: Option<String>,
}

fn ui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    rx: &mpsc::Receiver<Update>,
    tmux: &Tmux,
    own_pane: Option<&str>,
) -> Result<()> {
    let mut view = View {
        app: App::default(),
        error: None,
        offset: 0,
        home: env_var("HOME"),
    };
    let mut session_checked: Option<Instant> = None;
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
        if session_checked.is_none_or(|t| t.elapsed() >= SESSION_REFRESH) {
            session_checked = Some(Instant::now());
            if let Some(pane) = own_pane {
                let where_ = "#{session_name}\t#{window_id}";
                match tmux.run(&["display-message", "-p", "-t", pane, where_]) {
                    Ok(out) => match out.trim_end().split_once('\t') {
                        Some((session, window)) => {
                            view.app.set_location(session.to_owned(), window.to_owned());
                        }
                        None => view.error = Some(format!("unexpected tmux reply: {out:?}")),
                    },
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
        if let Action::Jump(pane) = action
            && let Err(e) = jump(tmux, &pane)
        {
            view.error = Some(format!("{e:#}"));
        }
    }
}

fn jump(tmux: &Tmux, pane: &str) -> Result<()> {
    tmux.run(&["switch-client", "-t", pane])?;
    tmux.run(&["select-window", "-t", pane])?;
    tmux.run(&["select-pane", "-t", pane])?;
    Ok(())
}

fn state_color(state: State) -> Color {
    match state {
        State::Blocked => Color::Red,
        State::Done => Color::Green,
        State::Working => Color::Yellow,
        State::Idle => Color::Blue,
        State::Unknown => Color::DarkGray,
    }
}

/// Width of the ` <symbol> ` prefix, which the lines under an agent indent to match.
const PREFIX: usize = 3;

fn render(f: &mut Frame, view: &mut View) {
    let [list, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let width = usize::from(list.width).saturating_sub(PREFIX);
    let lines = view.app.lines();
    let selected_pane = view.app.selected.and_then(|i| {
        lines
            .iter()
            .filter_map(|l| match l {
                Line::Agent(r) => Some(r.pane.as_str()),
                _ => None,
            })
            .nth(i)
    });
    let current = Style::new().add_modifier(Modifier::BOLD);
    // Matches the session name in tmux's status-left.
    let own_session = Style::new().fg(Color::Rgb(0xf6, 0xc1, 0x77));
    let muted = Style::new().fg(Color::DarkGray);
    let detail = |pane: &str| {
        if selected_pane == Some(pane) {
            Style::new()
        } else {
            muted
        }
    };
    let mut selected_line = None;
    let rendered: Vec<TextLine> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| match line {
            Line::Session { name, current: own } => TextLine::from(Span::styled(
                model::truncate(name, usize::from(list.width)),
                if *own { own_session } else { muted },
            )),
            Line::Agent(row) => {
                let selected = selected_pane == Some(row.pane.as_str());
                if selected {
                    selected_line = Some(i);
                }
                TextLine::from(vec![
                    Span::styled(
                        format!(" {} ", model::symbol(row.state)),
                        Style::new().fg(state_color(row.state)),
                    ),
                    Span::styled(
                        model::agent_text(row, width),
                        if selected { current } else { muted },
                    ),
                ])
            }
            Line::Cwd { pane, cwd } => TextLine::from(Span::styled(
                format!(
                    "{:PREFIX$}{}",
                    "",
                    model::short_path(cwd, view.home.as_deref(), width)
                ),
                detail(pane),
            )),
            Line::Branch { pane, branch } => TextLine::from(Span::styled(
                format!("{:PREFIX$}{}", "", model::truncate(branch, width)),
                detail(pane),
            )),
        })
        .collect();
    let height = usize::from(list.height);
    if let Some(line) = selected_line {
        // Keep all lines of a selected agent in view.
        let last = line
            + lines[line + 1..]
                .iter()
                .take_while(|l| matches!(l, Line::Cwd { .. } | Line::Branch { .. }))
                .count();
        if line < view.offset {
            view.offset = line;
        } else if height > 0 && last >= view.offset + height {
            view.offset = last + 1 - height;
        }
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
