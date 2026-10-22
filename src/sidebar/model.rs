use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cli::ls::{fmt_age, location};
use crate::daemon::state::{PaneRecord, State};
use crate::protocol::Status;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StateFilter {
    #[default]
    All,
    Only(State),
}

impl StateFilter {
    pub fn next(self) -> StateFilter {
        match self {
            StateFilter::All => StateFilter::Only(State::Blocked),
            StateFilter::Only(State::Blocked) => StateFilter::Only(State::Done),
            StateFilter::Only(State::Done) => StateFilter::Only(State::Working),
            StateFilter::Only(State::Working) => StateFilter::Only(State::Idle),
            StateFilter::Only(_) => StateFilter::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StateFilter::All => "all",
            StateFilter::Only(s) => s.name(),
        }
    }

    fn admits(self, state: State) -> bool {
        match self {
            StateFilter::All => true,
            StateFilter::Only(s) => s == state,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub pane: String,
    pub state: State,
    pub tag: char,
    pub location: String,
    pub branch: String,
    pub age: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Session(String),
    Agent(Row),
}

/// Groups agents by session, the sidebar's own session first, then the rest by name.
pub fn lines(
    status: &Status,
    own_session: Option<&str>,
    filter: StateFilter,
    text: &str,
) -> Vec<Line> {
    let needle = text.to_lowercase();
    let mut recs: Vec<&PaneRecord> = status
        .panes
        .iter()
        .filter(|r| filter.admits(r.state))
        .filter(|r| needle.is_empty() || haystack(r).contains(&needle))
        .collect();
    recs.sort_by_key(|r| {
        let session = r.session.as_deref();
        (
            session.is_none(),
            session != own_session,
            session.map(str::to_owned),
            r.window,
            r.pane
                .trim_start_matches('%')
                .parse::<u32>()
                .unwrap_or(u32::MAX),
        )
    });
    let mut out = Vec::new();
    let mut current: Option<Option<&str>> = None;
    for r in recs {
        let session = r.session.as_deref();
        if current != Some(session) {
            current = Some(session);
            out.push(Line::Session(session.unwrap_or("?").to_owned()));
        }
        out.push(Line::Agent(Row {
            pane: r.pane.clone(),
            state: r.state,
            tag: r.agent.tag(),
            location: location(r),
            branch: r.branch.clone().unwrap_or_default(),
            age: fmt_age(status.now_ms.saturating_sub(r.since_ms)),
        }));
    }
    out
}

fn haystack(r: &PaneRecord) -> String {
    [
        Some(location(r)),
        r.branch.clone(),
        r.cwd.clone(),
        Some(r.state.name().to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Jump(String),
}

#[derive(Debug, Default)]
pub struct App {
    pub status: Option<Status>,
    pub own_session: Option<String>,
    pub filter: StateFilter,
    pub text: String,
    pub editing: bool,
    /// Index among agent rows, not lines.
    pub selected: usize,
}

impl App {
    pub fn lines(&self) -> Vec<Line> {
        match &self.status {
            Some(s) => lines(s, self.own_session.as_deref(), self.filter, &self.text),
            None => Vec::new(),
        }
    }

    fn agents(&self) -> Vec<Row> {
        self.lines()
            .into_iter()
            .filter_map(|l| match l {
                Line::Agent(r) => Some(r),
                Line::Session(_) => None,
            })
            .collect()
    }

    pub fn set_status(&mut self, status: Status) {
        self.status = Some(status);
        self.clamp();
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.agents().len().saturating_sub(1));
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if self.editing {
            match key.code {
                KeyCode::Enter => self.editing = false,
                KeyCode::Esc => {
                    self.editing = false;
                    self.text.clear();
                }
                KeyCode::Backspace => {
                    self.text.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.text.push(c);
                }
                _ => {}
            }
            self.clamp();
            return Action::None;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected += 1;
                self.clamp();
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('/') => self.editing = true,
            KeyCode::Char('f') => {
                self.filter = self.filter.next();
                self.selected = 0;
                self.clamp();
            }
            KeyCode::Enter => {
                if let Some(row) = self.agents().get(self.selected) {
                    return Action::Jump(row.pane.clone());
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Clicking line `index` (0-based, as rendered) selects and jumps to that agent.
    pub fn on_click(&mut self, index: usize) -> Action {
        let lines = self.lines();
        let Some(Line::Agent(row)) = lines.get(index) else {
            return Action::None;
        };
        self.selected = lines[..index]
            .iter()
            .filter(|l| matches!(l, Line::Agent(_)))
            .count();
        Action::Jump(row.pane.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pane: &str, state: State, session: &str, window: u32) -> PaneRecord {
        serde_json::from_value(serde_json::json!({
            "pane": pane, "agent": "claude", "state": state, "since_ms": 0,
            "last_event_ms": 0, "session_id": null, "cwd": "/w/repo",
            "branch": format!("br-{pane}"), "pending_tool": null,
            "session": session, "window": window,
        }))
        .unwrap()
    }

    fn status() -> Status {
        Status {
            now_ms: 125_000,
            sidebar: true,
            panes: vec![
                rec("%5", State::Idle, "beta", 1),
                rec("%1", State::Blocked, "alpha", 2),
                rec("%2", State::Done, "me", 3),
                rec("%3", State::Working, "me", 1),
            ],
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn panes(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .filter_map(|l| match l {
                Line::Agent(r) => Some(r.pane.as_str()),
                Line::Session(_) => None,
            })
            .collect()
    }

    #[test]
    fn own_session_first_then_by_name_and_window() {
        let out = lines(&status(), Some("me"), StateFilter::All, "");
        assert_eq!(out[0], Line::Session("me".into()));
        assert_eq!(panes(&out), ["%3", "%2", "%1", "%5"]);
        assert_eq!(out[3], Line::Session("alpha".into()));
        let Line::Agent(row) = &out[1] else { panic!() };
        assert_eq!(
            row,
            &Row {
                pane: "%3".into(),
                state: State::Working,
                tag: 'C',
                location: "me:1".into(),
                branch: "br-%3".into(),
                age: "2m".into(),
            }
        );
    }

    #[test]
    fn filters_by_state_and_text() {
        let out = lines(&status(), Some("me"), StateFilter::Only(State::Blocked), "");
        assert_eq!(
            out,
            vec![
                Line::Session("alpha".into()),
                lines(&status(), None, StateFilter::All, "")[1].clone()
            ]
        );
        let out = lines(&status(), Some("me"), StateFilter::All, "BR-%5");
        assert_eq!(panes(&out), ["%5"]);
        assert!(lines(&status(), None, StateFilter::All, "zzz").is_empty());
    }

    #[test]
    fn filter_cycles_through_states() {
        let mut f = StateFilter::All;
        let mut seen = vec![f.label()];
        for _ in 0..5 {
            f = f.next();
            seen.push(f.label());
        }
        assert_eq!(seen, ["all", "blocked", "done", "working", "idle", "all"]);
    }

    #[test]
    fn keys_move_and_jump() {
        let mut app = App {
            own_session: Some("me".into()),
            ..App::default()
        };
        app.set_status(status());
        assert_eq!(app.on_key(key('j')), Action::None);
        assert_eq!(app.on_key(key('j')), Action::None);
        assert_eq!(app.on_key(key('k')), Action::None);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.on_key(enter), Action::Jump("%2".into()));
        for _ in 0..10 {
            app.on_key(key('j'));
        }
        assert_eq!(app.selected, 3);
        app.on_key(key('q'));
        assert_eq!(app.selected, 3, "q is ignored");
    }

    #[test]
    fn slash_edits_text_filter() {
        let mut app = App::default();
        app.set_status(status());
        app.on_key(key('/'));
        assert!(app.editing);
        for c in "alp".chars() {
            app.on_key(key(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.text, "al");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.editing);
        assert_eq!(panes(&app.lines()), ["%1"]);
        app.on_key(key('/'));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.text, "");
        assert!(!app.editing);
    }

    #[test]
    fn f_cycles_filter_and_clamps_selection() {
        let mut app = App::default();
        app.set_status(status());
        app.selected = 3;
        app.on_key(key('f'));
        assert_eq!(app.filter, StateFilter::Only(State::Blocked));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn click_on_agent_jumps_and_on_header_does_nothing() {
        let mut app = App {
            own_session: Some("me".into()),
            ..App::default()
        };
        app.set_status(status());
        assert_eq!(app.on_click(0), Action::None);
        assert_eq!(app.on_click(2), Action::Jump("%2".into()));
        assert_eq!(app.selected, 1);
        assert_eq!(app.on_click(99), Action::None);
    }

    #[test]
    fn no_status_is_empty() {
        let mut app = App::default();
        assert!(app.lines().is_empty());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.on_key(enter), Action::None);
    }
}
