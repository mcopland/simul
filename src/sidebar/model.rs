use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cli::ls::fmt_age;
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
    /// `window_index:window_name`.
    pub label: String,
    pub branch: Option<String>,
    pub age: String,
    pub window_id: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Session(String),
    Agent(Row),
    /// Second line of an agent, naming its branch.
    Branch {
        pane: String,
        branch: String,
    },
}

/// Shapes, so states stay distinguishable without relying on color.
pub fn symbol(state: State) -> char {
    match state {
        State::Blocked => '!',
        State::Done => '\u{2713}',
        State::Working => '\u{25d0}',
        State::Idle => '\u{25cb}',
        State::Unknown => '?',
    }
}

pub fn label(rec: &PaneRecord) -> String {
    match (rec.window, rec.window_name.as_deref()) {
        (Some(w), Some(name)) if !name.is_empty() => format!("{w}:{name}"),
        (Some(w), _) => w.to_string(),
        (None, _) => "?".into(),
    }
}

/// Cuts `s` to at most `max` columns, marking the cut with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

/// The agent line after its symbol: the label, then the age right-aligned to `width`.
pub fn agent_text(row: &Row, width: usize) -> String {
    let room = width.saturating_sub(row.age.chars().count() + 1);
    let label = truncate(&row.label, room);
    format!("{label:<room$} {}", row.age)
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
            label: label(r),
            branch: r.branch.clone(),
            age: fmt_age(status.now_ms.saturating_sub(r.since_ms)),
            window_id: r.window_id.clone(),
            active: r.active,
        }));
        if let Some(branch) = &r.branch {
            out.push(Line::Branch {
                pane: r.pane.clone(),
                branch: branch.clone(),
            });
        }
    }
    out
}

fn haystack(r: &PaneRecord) -> String {
    [
        Some(label(r)),
        r.session.clone(),
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
    /// The window this sidebar lives in; its agent row is selected while unfocused.
    pub own_window: Option<String>,
    pub filter: StateFilter,
    pub text: String,
    pub editing: bool,
    /// Whether the sidebar pane has keyboard focus.
    pub focused: bool,
    /// Index among agent rows, not lines.
    pub selected: Option<usize>,
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
                _ => None,
            })
            .collect()
    }

    /// The agent row for this sidebar's window: its active pane if that is an agent,
    /// else the window's first agent.
    fn window_row(&self) -> Option<usize> {
        let window = self.own_window.as_deref()?;
        let agents = self.agents();
        let in_window = |r: &&Row| r.window_id.as_deref() == Some(window);
        let pick = agents
            .iter()
            .filter(in_window)
            .find(|r| r.active)
            .or_else(|| agents.iter().find(in_window))?;
        agents.iter().position(|r| r.pane == pick.pane)
    }

    /// Unfocused, selection mirrors the window; focused, it stays where the user put it.
    fn refresh(&mut self) {
        if self.focused {
            let len = self.agents().len();
            self.selected = self.selected.filter(|_| len > 0).map(|i| i.min(len - 1));
        } else {
            self.selected = self.window_row();
        }
    }

    pub fn set_status(&mut self, status: Status) {
        self.status = Some(status);
        self.refresh();
    }

    pub fn set_location(&mut self, session: String, window: String) {
        self.own_session = Some(session);
        self.own_window = Some(window);
        self.refresh();
    }

    pub fn on_focus(&mut self, gained: bool) {
        self.focused = gained;
        self.selected = self.window_row();
        if gained && self.selected.is_none() && !self.agents().is_empty() {
            self.selected = Some(0);
        }
    }

    fn step(&mut self, down: bool) {
        let len = self.agents().len();
        if len == 0 {
            return;
        }
        self.selected = Some(match self.selected {
            None => 0,
            Some(i) if down => (i + 1).min(len - 1),
            Some(i) => i.saturating_sub(1),
        });
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
            self.refresh();
            return Action::None;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.step(true),
            KeyCode::Char('k') | KeyCode::Up => self.step(false),
            KeyCode::Char('/') => self.editing = true,
            KeyCode::Char('f') => {
                self.filter = self.filter.next();
                self.selected = Some(0);
                self.refresh();
            }
            KeyCode::Enter => {
                let agents = self.agents();
                if let Some(row) = self.selected.and_then(|i| agents.get(i)) {
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
        let pane = match lines.get(index) {
            Some(Line::Agent(row)) => row.pane.clone(),
            Some(Line::Branch { pane, .. }) => pane.clone(),
            _ => return Action::None,
        };
        self.selected = self.agents().iter().position(|r| r.pane == pane);
        Action::Jump(pane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pane: &str, state: State, session: &str, window: u32, active: bool) -> PaneRecord {
        serde_json::from_value(serde_json::json!({
            "pane": pane, "agent": "claude", "state": state, "since_ms": 0,
            "last_event_ms": 0, "session_id": null, "cwd": "/w/repo",
            "branch": format!("br-{pane}"), "pending_tool": null,
            "session": session, "window": window,
            "window_id": format!("@{session}{window}"),
            "window_name": format!("w{window}"), "active": active,
        }))
        .unwrap()
    }

    fn status() -> Status {
        let mut no_branch = rec("%5", State::Idle, "beta", 1, true);
        no_branch.branch = None;
        Status {
            now_ms: 125_000,
            sidebar: true,
            panes: vec![
                no_branch,
                rec("%1", State::Blocked, "alpha", 2, true),
                rec("%2", State::Done, "me", 3, true),
                rec("%3", State::Working, "me", 1, true),
                rec("%4", State::Idle, "me", 3, false),
            ],
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn panes(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .filter_map(|l| match l {
                Line::Agent(r) => Some(r.pane.as_str()),
                _ => None,
            })
            .collect()
    }

    /// An app in session "me", window `window` (e.g. "@me3").
    fn app_in(window: &str) -> App {
        let mut app = App::default();
        app.set_location("me".into(), window.into());
        app.set_status(status());
        app
    }

    #[test]
    fn symbols_are_distinct_shapes() {
        let all = [
            State::Blocked,
            State::Done,
            State::Working,
            State::Idle,
            State::Unknown,
        ]
        .map(symbol);
        assert_eq!(all, ['!', '\u{2713}', '\u{25d0}', '\u{25cb}', '?']);
    }

    #[test]
    fn rows_have_window_label_and_branch_line() {
        let out = lines(&status(), Some("me"), StateFilter::All, "");
        assert_eq!(out[0], Line::Session("me".into()));
        assert_eq!(panes(&out), ["%3", "%2", "%4", "%1", "%5"]);
        let Line::Agent(row) = &out[1] else {
            panic!("{out:?}")
        };
        assert_eq!(row.label, "1:w1");
        assert_eq!(row.age, "2m");
        assert_eq!(row.branch.as_deref(), Some("br-%3"));
        assert_eq!(
            out[2],
            Line::Branch {
                pane: "%3".into(),
                branch: "br-%3".into()
            }
        );
        let last = out.last().unwrap();
        assert!(
            matches!(last, Line::Agent(r) if r.pane == "%5"),
            "no branch, no second line"
        );
    }

    #[test]
    fn label_falls_back_without_window_name() {
        let mut r = rec("%1", State::Idle, "s", 4, true);
        assert_eq!(label(&r), "4:w4");
        r.window_name = None;
        assert_eq!(label(&r), "4");
        r.window = None;
        assert_eq!(label(&r), "?");
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("main", 10), "main");
        assert_eq!(truncate("abcdef", 4), "abc\u{2026}");
        assert_eq!(truncate("abcdef", 0), "");
    }

    #[test]
    fn agent_text_right_aligns_age() {
        let mut row = match &lines(&status(), Some("me"), StateFilter::All, "")[1] {
            Line::Agent(r) => r.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(agent_text(&row, 10), "1:w1    2m");
        row.label = "1:a-very-long-name".into();
        assert_eq!(agent_text(&row, 12), "1:a-very\u{2026} 2m");
    }

    #[test]
    fn filters_by_state_and_text() {
        let out = lines(&status(), Some("me"), StateFilter::Only(State::Blocked), "");
        assert_eq!(panes(&out), ["%1"]);
        let out = lines(&status(), Some("me"), StateFilter::All, "BR-%1");
        assert_eq!(panes(&out), ["%1"]);
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
    fn selection_follows_own_window() {
        assert_eq!(app_in("@me1").selected, Some(0));
        assert_eq!(
            app_in("@me3").selected,
            Some(1),
            "the active pane of the window"
        );
        assert_eq!(app_in("@me9").selected, None, "no Claude in this window");
    }

    #[test]
    fn unfocused_selection_tracks_status_updates() {
        let mut app = app_in("@me9");
        let mut s = status();
        s.panes.push(rec("%7", State::Working, "me", 9, true));
        app.set_status(s);
        assert_eq!(app.selected, Some(3));
    }

    #[test]
    fn focus_without_agent_selects_first_row() {
        let mut app = app_in("@me9");
        app.on_focus(true);
        assert_eq!(app.selected, Some(0));
        app.on_focus(false);
        assert_eq!(app.selected, None);
    }

    #[test]
    fn focus_keeps_window_row_and_keys_move_freely() {
        let mut app = app_in("@me3");
        app.on_focus(true);
        assert_eq!(app.selected, Some(1));
        app.on_key(key('j'));
        app.on_key(key('j'));
        assert_eq!(app.selected, Some(3));
        app.set_status(status());
        assert_eq!(
            app.selected,
            Some(3),
            "updates don't move a focused selection"
        );
        assert_eq!(app.on_key(enter()), Action::Jump("%1".into()));
        for _ in 0..10 {
            app.on_key(key('j'));
        }
        assert_eq!(app.selected, Some(4));
        app.on_key(key('k'));
        assert_eq!(app.selected, Some(3));
        app.on_key(key('q'));
        assert_eq!(app.selected, Some(3), "q is ignored");
        app.on_focus(false);
        assert_eq!(app.selected, Some(1), "snaps back to the window's agent");
    }

    #[test]
    fn keys_from_no_selection_start_at_top() {
        let mut app = app_in("@me9");
        assert_eq!(app.on_key(enter()), Action::None);
        app.on_key(key('j'));
        assert_eq!(app.selected, Some(0));
    }

    #[test]
    fn slash_edits_text_filter() {
        let mut app = app_in("@me1");
        app.on_focus(true);
        app.on_key(key('/'));
        assert!(app.editing);
        for c in "alp".chars() {
            app.on_key(key(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.text, "al");
        app.on_key(enter());
        assert!(!app.editing);
        assert_eq!(panes(&app.lines()), ["%1"]);
        assert_eq!(app.selected, Some(0));
        app.on_key(key('/'));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.text, "");
        assert!(!app.editing);
    }

    #[test]
    fn f_cycles_filter_and_clamps_selection() {
        let mut app = app_in("@me1");
        app.on_focus(true);
        app.selected = Some(4);
        app.on_key(key('f'));
        assert_eq!(app.filter, StateFilter::Only(State::Blocked));
        assert_eq!(app.selected, Some(0));
    }

    #[test]
    fn click_on_agent_or_branch_line_jumps() {
        let mut app = app_in("@me9");
        assert_eq!(app.on_click(0), Action::None, "session header");
        assert_eq!(app.on_click(3), Action::Jump("%2".into()));
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.on_click(4), Action::Jump("%2".into()), "branch line");
        assert_eq!(app.on_click(99), Action::None);
    }

    #[test]
    fn no_status_is_empty() {
        let mut app = App::default();
        assert!(app.lines().is_empty());
        assert_eq!(app.on_key(enter()), Action::None);
        app.on_focus(true);
        assert_eq!(app.selected, None);
    }
}
