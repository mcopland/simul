use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cli::ls::fmt_age;
use crate::daemon::state::{PaneRecord, State};
use crate::protocol::{GitStatus, PaneSummary, Status, WindowInfo};

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
            StateFilter::Only(State::Blocked) => StateFilter::Only(State::Asking),
            StateFilter::Only(State::Asking) => StateFilter::Only(State::Failed),
            StateFilter::Only(State::Failed) => StateFilter::Only(State::Done),
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
    /// The agent's name.
    pub label: String,
    pub age: String,
    pub window_id: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Session {
        name: String,
    },
    /// A window header; `id` is what jumping to it targets.
    Window {
        id: String,
        label: String,
    },
    Agent(Row),
    /// Line under an agent: where it is, as `place` puts it.
    Place {
        pane: String,
        text: String,
    },
    /// A pane without an agent, named by where it is, as `place` puts it.
    Pane {
        id: String,
        place: String,
        window: String,
        active: bool,
    },
}

/// Shapes, so states stay distinguishable without relying on color.
pub fn symbol(state: State) -> char {
    match state {
        State::Blocked => '\u{2298}',
        State::Asking => '\u{003f}',
        State::Done => '\u{25cf}',
        State::Failed => '\u{2717}',
        State::Working => '\u{25d0}',
        State::Idle => '\u{25cb}',
        State::Unknown => '\u{2205}',
    }
}

/// A selectable line: what j/k step over and Enter jumps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Window(String),
    Agent(Row),
    Pane {
        id: String,
        window: String,
        active: bool,
    },
}

impl Item {
    /// The same row: a placeholder header can share its target with the agent under it.
    fn same(&self, other: &Item) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
            && self.target() == other.target()
    }

    pub fn target(&self) -> &str {
        match self {
            Item::Window(id) | Item::Pane { id, .. } => id,
            Item::Agent(row) => &row.pane,
        }
    }

    fn in_window(&self, window: &str) -> bool {
        match self {
            Item::Window(_) => false,
            Item::Agent(r) => r.window_id.as_deref() == Some(window),
            Item::Pane { window: w, .. } => w == window,
        }
    }

    fn active(&self) -> bool {
        match self {
            Item::Window(_) => false,
            Item::Agent(r) => r.active,
            Item::Pane { active, .. } => *active,
        }
    }
}

pub fn label(rec: &PaneRecord) -> String {
    window_label(rec.window, rec.window_name.as_deref())
}

fn window_label(index: Option<u32>, name: Option<&str>) -> String {
    match (index, name) {
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

/// Where a pane is, like the starship prompt: the branch and its status markers in a repo,
/// else the directory as `short_path` puts it.
pub fn place(cwd: &str, git: Option<&GitStatus>, home: &str) -> String {
    let Some(g) = git else {
        return short_path(cwd, home);
    };
    let ahead_behind = match (g.ahead > 0, g.behind > 0) {
        (true, true) => Some('\u{21d5}'),
        (true, false) => Some('\u{2303}'),
        (false, true) => Some('\u{2304}'),
        (false, false) => None,
    };
    let markers = [
        (g.conflicted > 0).then_some('!'),
        ahead_behind,
        (g.staged > 0).then_some('+'),
        (g.deleted > 0).then_some('-'),
        (g.modified > 0).then_some('*'),
        (g.untracked > 0).then_some('?'),
        (g.stashed > 0).then_some('\u{2261}'),
    ];
    let mut out = g.branch.clone().unwrap_or_else(|| "HEAD".into());
    for m in markers.into_iter().flatten() {
        out.push(' ');
        out.push(m);
    }
    out
}

/// `path` with each parent cut to its first letter (a dotted name keeps its dot too) and
/// written from `home` when under it: `~/dev/simul/examples` is `/d/s/examples`, and home is `~`.
pub fn short_path(path: &str, home: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let home = home.trim_end_matches('/');
    let rest = match path.strip_prefix(home) {
        _ if home.is_empty() => path,
        Some("" | "/") => return "~".into(),
        Some(rest) if rest.starts_with('/') => rest,
        _ => path,
    };
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    let Some((last, parents)) = parts.split_last() else {
        return "/".into();
    };
    let mut out = String::new();
    for p in parents {
        let keep = if p.starts_with('.') { 2 } else { 1 };
        out.push('/');
        out.extend(p.chars().take(keep));
    }
    out.push('/');
    out.push_str(last);
    out
}

/// The agent line after its symbol: the label, then the age right-aligned to `width`.
pub fn agent_text(row: &Row, width: usize) -> String {
    let room = width.saturating_sub(row.age.chars().count() + 1);
    let label = truncate(&row.label, room);
    format!("{label:<room$} {}", row.age)
}

struct Window<'a> {
    id: String,
    session: Option<&'a str>,
    index: Option<u32>,
    label: String,
    panes: &'a [PaneSummary],
    agents: Vec<&'a PaneRecord>,
}

/// A row under a window header; an agent has its pane's summary unless the window lacks it.
enum Entry<'a> {
    Agent(&'a PaneRecord, Option<&'a PaneSummary>),
    Pane(&'a PaneSummary),
}

/// An entry with its `place` text.
type Placed<'a> = (Entry<'a>, Option<String>);

impl Window<'_> {
    fn haystack(&self) -> String {
        format!("{} {}", self.session.unwrap_or_default(), self.label).to_lowercase()
    }
}

/// Lists every window under its session, and each pane under its window, as an agent row when
/// an agent runs there. Sessions sort by name so rows don't jump as focus moves. `home` shortens
/// paths (see `short_path`).
pub fn lines(status: &Status, filter: StateFilter, text: &str, home: &str) -> Vec<Line> {
    let needle = text.to_lowercase();
    let mut windows: Vec<Window> = status.windows.iter().map(window_of).collect();
    for r in &status.panes {
        // An agent the daemon has not placed in a known window still gets a header.
        let id = match (&r.window_id, &r.session, r.window) {
            (Some(id), _, _) => id.clone(),
            // `=` makes tmux match the session name exactly, even one like "3".
            (None, Some(session), Some(window)) => format!("={session}:{window}"),
            _ => r.pane.clone(),
        };
        let pos = match windows.iter().position(|w| w.id == id) {
            Some(pos) => pos,
            None => {
                windows.push(Window {
                    id,
                    session: r.session.as_deref(),
                    index: r.window,
                    label: label(r),
                    panes: &[],
                    agents: Vec::new(),
                });
                windows.len() - 1
            }
        };
        windows[pos].agents.push(r);
    }
    let mut windows: Vec<(Window, Vec<Placed>)> = windows
        .into_iter()
        .filter_map(|w| {
            let hit = needle.is_empty() || w.haystack().contains(&needle);
            let entries: Vec<Placed> = entries(&w)
                .into_iter()
                .map(|e| {
                    let place = entry_place(&e, home);
                    (e, place)
                })
                .filter(|(e, place)| {
                    let found = |s: &str| s.to_lowercase().contains(&needle);
                    let place_hit = place.as_deref().is_some_and(found);
                    match e {
                        Entry::Agent(r, _) => {
                            filter.admits(r.state) && (hit || place_hit || found(&haystack(r)))
                        }
                        // A state filter is about agents, so it hides plain panes.
                        Entry::Pane(p) => {
                            filter == StateFilter::All && (hit || place_hit || found(&p.command))
                        }
                    }
                })
                .collect();
            let shown = !entries.is_empty() || (hit && filter == StateFilter::All);
            shown.then_some((w, entries))
        })
        .collect();
    windows.sort_by_key(|(w, _)| {
        (
            w.session.is_none(),
            w.session.map(str::to_owned),
            w.index.is_none(),
            w.index,
        )
    });
    let mut out = Vec::new();
    let mut current: Option<Option<&str>> = None;
    for (w, entries) in windows {
        if current != Some(w.session) {
            current = Some(w.session);
            out.push(Line::Session {
                name: w.session.unwrap_or("?").to_owned(),
            });
        }
        out.push(Line::Window {
            id: w.id.clone(),
            label: w.label.clone(),
        });
        for (e, place) in entries {
            let r = match e {
                Entry::Agent(r, _) => r,
                Entry::Pane(p) => {
                    out.push(Line::Pane {
                        id: p.id.clone(),
                        place: place.unwrap_or_default(),
                        window: w.id.clone(),
                        active: p.active,
                    });
                    continue;
                }
            };
            out.push(Line::Agent(Row {
                pane: r.pane.clone(),
                state: r.state,
                label: r.agent.name().to_owned(),
                age: fmt_age(status.now_ms.saturating_sub(r.since_ms)),
                window_id: r.window_id.clone(),
                active: r.active,
            }));
            if let Some(text) = place {
                out.push(Line::Place {
                    pane: r.pane.clone(),
                    text,
                });
            }
        }
    }
    out
}

/// The entry's `place`; an agent outside known panes falls back to its own branch, then cwd.
fn entry_place(e: &Entry, home: &str) -> Option<String> {
    let text = match e {
        Entry::Pane(p) | Entry::Agent(_, Some(p)) => place(&p.cwd, p.git.as_ref(), home),
        Entry::Agent(r, None) => match (&r.branch, &r.cwd) {
            (Some(branch), _) => branch.clone(),
            (None, Some(cwd)) => short_path(cwd, home),
            (None, None) => String::new(),
        },
    };
    (!text.is_empty()).then_some(text)
}

fn window_of(w: &WindowInfo) -> Window<'_> {
    Window {
        id: w.id.clone(),
        session: Some(&w.session),
        index: Some(w.index),
        label: window_label(Some(w.index), Some(&w.name)),
        panes: &w.panes,
        agents: Vec::new(),
    }
}

/// The window's panes in tmux's order, then agents in no known pane, by pane number.
fn entries<'a>(w: &Window<'a>) -> Vec<Entry<'a>> {
    let mut out: Vec<Entry> = w
        .panes
        .iter()
        .map(|p| match w.agents.iter().find(|r| r.pane == p.id) {
            Some(r) => Entry::Agent(r, Some(p)),
            None => Entry::Pane(p),
        })
        .collect();
    let mut rest: Vec<&PaneRecord> = w
        .agents
        .iter()
        .copied()
        .filter(|r| !w.panes.iter().any(|p| p.id == r.pane))
        .collect();
    rest.sort_by_key(|r| {
        r.pane
            .trim_start_matches('%')
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    out.extend(rest.into_iter().map(|r| Entry::Agent(r, None)));
    out
}

fn haystack(r: &PaneRecord) -> String {
    // Session and window names are matched per window.
    [
        Some(r.agent.name().to_owned()),
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

fn selectable(line: &Line) -> bool {
    matches!(
        line,
        Line::Window { .. } | Line::Agent(_) | Line::Pane { .. }
    )
}

/// The index in `lines` of selectable line `n`.
pub fn line_of(lines: &[Line], n: usize) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| selectable(l))
        .nth(n)
        .map(|(i, _)| i)
}

/// The scroll offset that shows line `line` with the headers above it and the place line
/// below it, moving `offset` as little as possible.
pub fn scroll(lines: &[Line], line: usize, offset: usize, height: usize) -> usize {
    let mut first = line;
    if matches!(lines.get(line), Some(Line::Agent(_) | Line::Pane { .. }))
        && matches!(
            first.checked_sub(1).map(|i| &lines[i]),
            Some(Line::Window { .. })
        )
    {
        first -= 1;
    }
    if matches!(
        first.checked_sub(1).map(|i| &lines[i]),
        Some(Line::Session { .. })
    ) {
        first -= 1;
    }
    let last = line
        + lines[line + 1..]
            .iter()
            .take_while(|l| matches!(l, Line::Place { .. }))
            .count();
    if first < offset {
        first
    } else if height > 0 && last >= offset + height {
        last + 1 - height
    } else {
        offset
    }
}

/// The selectable lines in render order.
pub fn items(lines: &[Line]) -> Vec<Item> {
    lines
        .iter()
        .filter_map(|l| match l {
            Line::Window { id, .. } => Some(Item::Window(id.clone())),
            Line::Agent(r) => Some(Item::Agent(r.clone())),
            Line::Pane {
                id, window, active, ..
            } => Some(Item::Pane {
                id: id.clone(),
                window: window.clone(),
                active: *active,
            }),
            _ => None,
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Jump(String),
}

#[derive(Debug, Default)]
pub struct App {
    pub status: Option<Status>,
    /// The window this sidebar lives in; its active pane's row is selected while unfocused.
    pub own_window: Option<String>,
    pub filter: StateFilter,
    pub text: String,
    pub editing: bool,
    /// Whether the sidebar pane has keyboard focus.
    pub focused: bool,
    /// Index among selectable rows (see `items`), not lines.
    pub selected: Option<usize>,
    /// `$HOME`, which shortened paths start from.
    pub home: String,
}

impl App {
    pub fn lines(&self) -> Vec<Line> {
        match &self.status {
            Some(s) => lines(s, self.filter, &self.text, &self.home),
            None => Vec::new(),
        }
    }

    fn items(&self) -> Vec<Item> {
        items(&self.lines())
    }

    /// The row for this sidebar's window: its active pane, else its first agent, else the
    /// window itself.
    fn window_row(&self) -> Option<usize> {
        let window = self.own_window.as_deref()?;
        let items = self.items();
        let pick = items
            .iter()
            .find(|i| i.in_window(window) && i.active())
            .or_else(|| {
                items
                    .iter()
                    .find(|i| i.in_window(window) && matches!(i, Item::Agent(_)))
            })
            .or_else(|| {
                items
                    .iter()
                    .find(|i| matches!(i, Item::Window(id) if id == window))
            })?;
        items.iter().position(|i| i == pick)
    }

    /// Unfocused, selection mirrors the window; focused, it stays where the user put it.
    fn refresh(&mut self) {
        if self.focused {
            let len = self.items().len();
            self.selected = self.selected.filter(|_| len > 0).map(|i| i.min(len - 1));
        } else {
            self.selected = self.window_row();
        }
    }

    /// While focused, the selection stays on its window or agent as rows above it come and go.
    pub fn set_status(&mut self, status: Status) {
        let kept = self
            .focused
            .then(|| self.selected.and_then(|i| self.items().into_iter().nth(i)))
            .flatten();
        self.status = Some(status);
        if let Some(item) = kept
            && let Some(i) = self.items().iter().position(|i| i.same(&item))
        {
            self.selected = Some(i);
            return;
        }
        self.refresh();
    }

    pub fn set_location(&mut self, window: String) {
        self.own_window = Some(window);
        self.refresh();
    }

    pub fn on_focus(&mut self, gained: bool) {
        self.focused = gained;
        self.selected = self.window_row();
        if gained && self.selected.is_none() && !self.items().is_empty() {
            self.selected = Some(0);
        }
    }

    fn step(&mut self, down: bool) {
        let len = self.items().len();
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
                let items = self.items();
                if let Some(item) = self.selected.and_then(|i| items.get(i)) {
                    return Action::Jump(item.target().to_owned());
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Clicking line `index` (0-based, as rendered) selects and jumps to that window or pane.
    pub fn on_click(&mut self, index: usize) -> Action {
        let lines = self.lines();
        let line = match lines.get(index) {
            // A place line directly follows its agent.
            Some(Line::Place { .. }) => index.saturating_sub(1),
            Some(l) if selectable(l) => index,
            _ => return Action::None,
        };
        let n = lines[..line].iter().filter(|l| selectable(l)).count();
        let Some(item) = items(&lines).into_iter().nth(n) else {
            return Action::None;
        };
        self.selected = Some(n);
        Action::Jump(item.target().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PaneSummary;

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

    const HOME: &str = "/h";

    /// Lines, with item numbers in brackets:
    /// 0 me | 1 [0] 1:w1 | 2 [1] %3 | 3 br | 4 [2] %10 zsh | 5 [3] 2:notes | 6 [4] %11 zsh |
    /// 7 [5] 3:w3 | 8 [6] %2 | 9 br | 10 [7] %4 | 11 br | 12 [8] 9:w9 | 13 [9] %12 vim |
    /// 14 ops | 15 [10] 2:w2 | 16 [11] %1 | 17 br | 18 web | 19 [12] 1:w1 | 20 [13] %5 |
    /// 21 dir | 22 zz | 23 [14] 1:logs | 24 [15] %13 tail
    ///
    /// Agents are in a repo on branch `br-<pane>`, except %5; other panes are in `~/src/<cmd>`.
    fn status() -> Status {
        let mut no_branch = rec("%5", State::Idle, "web", 1, true);
        no_branch.branch = None;
        let mut s = Status {
            now_ms: 125_000,
            sidebar: true,
            panes: vec![
                no_branch,
                rec("%1", State::Blocked, "ops", 2, true),
                rec("%2", State::Done, "me", 3, true),
                rec("%3", State::Working, "me", 1, true),
                rec("%4", State::Idle, "me", 3, false),
            ],
            windows: vec![
                win(
                    "me",
                    1,
                    "w1",
                    &[("%3", "claude", true), ("%10", "zsh", false)],
                ),
                win("me", 2, "notes", &[("%11", "zsh", true)]),
                win(
                    "me",
                    3,
                    "w3",
                    &[("%2", "claude", true), ("%4", "claude", false)],
                ),
                win("me", 9, "w9", &[("%12", "vim", true)]),
                win("ops", 2, "w2", &[("%1", "claude", true)]),
                win("web", 1, "w1", &[("%5", "claude", true)]),
                win("zz", 1, "logs", &[("%13", "tail", true)]),
            ],
        };
        for p in s.windows.iter_mut().flat_map(|w| &mut w.panes) {
            if p.command == "claude" && p.id != "%5" {
                p.cwd = "/w/repo".into();
                p.git = Some(GitStatus {
                    branch: Some(format!("br-{}", p.id)),
                    ..GitStatus::default()
                });
            }
        }
        s
    }

    fn win(session: &str, index: u32, name: &str, panes: &[(&str, &str, bool)]) -> WindowInfo {
        WindowInfo {
            id: format!("@{session}{index}"),
            session: session.into(),
            index,
            name: name.into(),
            panes: panes
                .iter()
                .map(|&(id, command, active)| PaneSummary {
                    id: id.into(),
                    command: command.into(),
                    active,
                    cwd: format!("{HOME}/src/{command}"),
                    git: None,
                })
                .collect(),
        }
    }

    fn window(id: &str, label: &str) -> Line {
        Line::Window {
            id: id.into(),
            label: label.into(),
        }
    }

    fn session(name: &str) -> Line {
        Line::Session { name: name.into() }
    }

    /// A plain pane line, in `~/src/<command>` like the fixture's.
    fn pane(id: &str, command: &str, window: &str, active: bool) -> Line {
        Line::Pane {
            id: id.into(),
            place: format!("/s/{command}"),
            window: window.into(),
            active,
        }
    }

    fn all(status: &Status) -> Vec<Line> {
        lines(status, StateFilter::All, "", HOME)
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

    /// An app in window `window` (e.g. "@me3").
    fn app_in(window: &str) -> App {
        let mut app = App {
            home: HOME.into(),
            ..App::default()
        };
        app.set_location(window.into());
        app.set_status(status());
        app
    }

    #[test]
    fn symbols_are_distinct_shapes() {
        let all = [
            State::Blocked,
            State::Asking,
            State::Done,
            State::Failed,
            State::Working,
            State::Idle,
            State::Unknown,
        ]
        .map(symbol);
        assert_eq!(
            all,
            [
                '\u{2298}', '\u{003f}', '\u{25cf}', '\u{2717}', '\u{25d0}', '\u{25cb}', '\u{2205}'
            ]
        );
    }

    #[test]
    fn rows_nest_agents_under_window_headers() {
        let out = all(&status());
        assert_eq!(out[0], session("me"));
        assert_eq!(panes(&out), ["%3", "%2", "%4", "%1", "%5"]);
        assert_eq!(out[1], window("@me1", "1:w1"));
        let Line::Agent(row) = &out[2] else {
            panic!("{out:?}")
        };
        assert_eq!(row.label, "claude");
        assert_eq!(row.age, "2m");
        assert_eq!(
            out[3],
            Line::Place {
                pane: "%3".into(),
                text: "br-%3".into()
            }
        );
        let web = out.iter().position(|l| *l == session("web")).unwrap();
        assert!(
            matches!(&out[web + 2], Line::Agent(r) if r.pane == "%5"),
            "{out:?}"
        );
        assert_eq!(
            out[web + 3],
            Line::Place {
                pane: "%5".into(),
                text: "/s/claude".into()
            },
            "outside git, the directory"
        );
    }

    #[test]
    fn unplaced_agent_falls_back_to_its_branch_then_cwd() {
        let mut s = status();
        s.windows.clear();
        s.panes.truncate(2);
        s.panes[0].cwd = Some("/h/x/y".into());
        let out = all(&s);
        let places: Vec<&str> = out
            .iter()
            .filter_map(|l| match l {
                Line::Place { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(places, ["br-%1", "/x/y"], "{out:?}");
    }

    #[test]
    fn place_shows_branch_and_starship_markers() {
        let mut g = GitStatus {
            branch: Some("main".into()),
            ..GitStatus::default()
        };
        assert_eq!(place("/h/r", Some(&g), HOME), "main");
        g.ahead = 1;
        assert_eq!(place("/h/r", Some(&g), HOME), "main \u{2303}");
        g.ahead = 0;
        g.behind = 2;
        assert_eq!(place("/h/r", Some(&g), HOME), "main \u{2304}");
        g = GitStatus {
            branch: None,
            ahead: 1,
            behind: 1,
            conflicted: 1,
            staged: 1,
            deleted: 1,
            modified: 1,
            untracked: 1,
            stashed: 1,
        };
        assert_eq!(
            place("/h/r", Some(&g), HOME),
            "HEAD ! \u{21d5} + - * ? \u{2261}"
        );
        assert_eq!(place("/h/r", None, HOME), "/r", "no git: the directory");
    }

    #[test]
    fn short_path_abbreviates_parents() {
        let home = "/Users/me";
        assert_eq!(short_path("/Users/me", home), "~");
        assert_eq!(short_path("/Users/me/", "/Users/me/"), "~");
        assert_eq!(short_path("/Users/me/dev", home), "/dev");
        assert_eq!(
            short_path("/Users/me/dev/simul/examples", home),
            "/d/s/examples"
        );
        assert_eq!(short_path("/Users/me/.config/simul", home), "/.c/simul");
        assert_eq!(
            short_path("/Users/meme/x", home),
            "/U/m/x",
            "not under home"
        );
        assert_eq!(short_path("/usr/local/bin", home), "/u/l/bin");
        assert_eq!(short_path("/", home), "/");
        assert_eq!(short_path("/usr/local", ""), "/u/local", "no home");
        assert_eq!(short_path("", home), "");
    }

    #[test]
    fn windows_and_sessions_without_agents_are_listed() {
        let out = all(&status());
        assert_eq!(out[5], window("@me2", "2:notes"));
        assert_eq!(out[7], window("@me3", "3:w3"));
        assert_eq!(out[12], window("@me9", "9:w9"));
        assert_eq!(
            out[out.len() - 3..],
            [
                session("zz"),
                window("@zz1", "1:logs"),
                pane("%13", "tail", "@zz1", true)
            ]
        );
    }

    #[test]
    fn plain_panes_are_listed_in_pane_order() {
        let mut s = status();
        s.windows[0].panes.reverse();
        let out = all(&s);
        assert_eq!(out[2], pane("%10", "zsh", "@me1", false));
        assert!(
            matches!(&out[3], Line::Agent(r) if r.pane == "%3"),
            "{out:?}"
        );
        assert_eq!(all(&status())[13], pane("%12", "vim", "@me9", true));
    }

    #[test]
    fn agent_outside_known_windows_gets_its_own_header() {
        let mut s = status();
        s.windows.retain(|w| w.id != "@web1");
        for pane in ["%8", "%9"] {
            let mut unplaced = rec(pane, State::Idle, "web", 4, true);
            unplaced.window_id = None;
            s.panes.push(unplaced);
        }
        let out = all(&s);
        let web = out.iter().position(|l| *l == session("web")).unwrap();
        assert_eq!(out[web + 1], window("@web1", "1:w1"));
        assert_eq!(out[web + 4], window("=web:4", "4:w4"), "a tmux target");
        assert!(
            matches!(&out[web + 7], Line::Agent(r) if r.pane == "%9"),
            "{out:?}"
        );
        assert_eq!(
            out.iter()
                .filter(|l| **l == window("=web:4", "4:w4"))
                .count(),
            1,
            "one header per window"
        );
    }

    #[test]
    fn state_filter_hides_windows_without_a_match() {
        let out = lines(&status(), StateFilter::Only(State::Done), "", HOME);
        assert_eq!(
            out,
            [
                session("me"),
                window("@me3", "3:w3"),
                Line::Agent(Row {
                    pane: "%2".into(),
                    state: State::Done,
                    label: "claude".into(),
                    age: "2m".into(),
                    window_id: Some("@me3".into()),
                    active: true,
                }),
                Line::Place {
                    pane: "%2".into(),
                    text: "br-%2".into()
                },
            ]
        );
    }

    #[test]
    fn state_filter_drops_plain_panes() {
        let out = lines(&status(), StateFilter::Only(State::Working), "", HOME);
        assert_eq!(panes(&out), ["%3"]);
        assert!(
            !out.iter().any(|l| matches!(l, Line::Pane { .. })),
            "{out:?}"
        );
    }

    #[test]
    fn text_matching_a_window_keeps_all_its_agents() {
        let out = lines(&status(), StateFilter::All, "logs", HOME);
        assert_eq!(
            out,
            [
                session("zz"),
                window("@zz1", "1:logs"),
                pane("%13", "tail", "@zz1", true)
            ]
        );
        let out = lines(&status(), StateFilter::All, "3:W3", HOME);
        assert_eq!(panes(&out), ["%2", "%4"]);
        let out = lines(&status(), StateFilter::All, "CLAUDE", HOME);
        assert_eq!(panes(&out).len(), 5, "matches the agent name");
    }

    #[test]
    fn text_matches_a_pane_command() {
        let out = lines(&status(), StateFilter::All, "VIM", HOME);
        assert_eq!(
            out,
            [
                session("me"),
                window("@me9", "9:w9"),
                pane("%12", "vim", "@me9", true)
            ]
        );
    }

    #[test]
    fn text_matches_a_pane_place() {
        let out = lines(&status(), StateFilter::All, "/S/TAIL", HOME);
        assert_eq!(out.last(), Some(&pane("%13", "tail", "@zz1", true)));
        assert_eq!(out.len(), 3, "{out:?}");
    }

    #[test]
    fn cwd_still_matches_the_text_filter() {
        let out = lines(&status(), StateFilter::All, "/w/repo", HOME);
        assert_eq!(panes(&out).len(), 5);
    }

    #[test]
    fn sessions_sort_by_name() {
        let out = all(&status());
        let sessions: Vec<&str> = out
            .iter()
            .filter_map(|l| match l {
                Line::Session { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(sessions, ["me", "ops", "web", "zz"]);
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
        let mut row = match &all(&status())[2] {
            Line::Agent(r) => r.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(agent_text(&row, 10), "claude  2m");
        row.label = "1:a-very-long-name".into();
        assert_eq!(agent_text(&row, 12), "1:a-very\u{2026} 2m");
    }

    #[test]
    fn filters_by_state_and_text() {
        let out = lines(&status(), StateFilter::Only(State::Blocked), "", HOME);
        assert_eq!(panes(&out), ["%1"]);
        let out = lines(&status(), StateFilter::All, "BR-%1", HOME);
        assert_eq!(panes(&out), ["%1"]);
        assert!(lines(&status(), StateFilter::All, "zzz", HOME).is_empty());
    }

    #[test]
    fn filter_cycles_through_states() {
        let mut f = StateFilter::All;
        let mut seen = vec![f.label()];
        for _ in 0..7 {
            f = f.next();
            seen.push(f.label());
        }
        assert_eq!(
            seen,
            [
                "all", "blocked", "asking", "failed", "done", "working", "idle", "all"
            ]
        );
    }

    #[test]
    fn selection_follows_own_window() {
        assert_eq!(app_in("@me1").selected, Some(1));
        assert_eq!(
            app_in("@me3").selected,
            Some(6),
            "the active pane of the window"
        );
        assert_eq!(app_in("@me2").selected, Some(4), "an active plain pane");
        let mut app = app_in("@me9");
        let mut s = status();
        s.windows[3].panes.clear();
        app.set_status(s);
        assert_eq!(app.selected, Some(8), "no panes: the window");
        assert_eq!(app_in("@gone").selected, None, "unknown window");
    }

    #[test]
    fn unfocused_selection_tracks_status_updates() {
        let mut app = app_in("@me9");
        let mut s = status();
        s.panes.push(rec("%7", State::Working, "me", 9, true));
        s.windows[3] = win(
            "me",
            9,
            "w9",
            &[("%12", "vim", false), ("%7", "claude", true)],
        );
        app.set_status(s);
        assert_eq!(app.selected, Some(10));
    }

    #[test]
    fn focus_without_agent_selects_first_row() {
        let mut app = app_in("@gone");
        app.on_focus(true);
        assert_eq!(app.selected, Some(0));
        app.on_focus(false);
        assert_eq!(app.selected, None);
    }

    #[test]
    fn focus_keeps_window_row_and_keys_move_freely() {
        let mut app = app_in("@me3");
        app.on_focus(true);
        assert_eq!(app.selected, Some(6));
        app.on_key(key('j'));
        app.on_key(key('j'));
        assert_eq!(app.selected, Some(8));
        app.set_status(status());
        assert_eq!(
            app.selected,
            Some(8),
            "updates don't move a focused selection"
        );
        assert_eq!(
            app.on_key(enter()),
            Action::Jump("@me9".into()),
            "a window header jumps to its window"
        );
        app.on_key(key('j'));
        assert_eq!(
            app.on_key(enter()),
            Action::Jump("%12".into()),
            "a plain pane jumps to itself"
        );
        app.on_key(key('j'));
        app.on_key(key('j'));
        assert_eq!(app.on_key(enter()), Action::Jump("%1".into()));
        for _ in 0..20 {
            app.on_key(key('j'));
        }
        assert_eq!(app.selected, Some(15));
        app.on_key(key('k'));
        assert_eq!(app.selected, Some(14));
        app.on_key(key('q'));
        assert_eq!(app.selected, Some(14), "q is ignored");
        app.on_focus(false);
        assert_eq!(app.selected, Some(6), "snaps back to the window's agent");
    }

    #[test]
    fn keys_from_no_selection_start_at_top() {
        let mut app = app_in("@gone");
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
        for c in "ops".chars() {
            app.on_key(key(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.text, "op");
        app.on_key(enter());
        assert!(!app.editing);
        assert_eq!(panes(&app.lines()), ["%1"]);
        assert_eq!(app.selected, Some(1), "clamped to the last row left");
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
    fn click_on_window_agent_or_place_line_jumps() {
        let mut app = app_in("@me9");
        assert_eq!(app.on_click(0), Action::None, "session header");
        assert_eq!(app.on_click(8), Action::Jump("%2".into()));
        assert_eq!(app.selected, Some(6));
        assert_eq!(app.on_click(9), Action::Jump("%2".into()), "place line");
        assert_eq!(
            app.on_click(5),
            Action::Jump("@me2".into()),
            "window header"
        );
        assert_eq!(app.selected, Some(3));
        assert_eq!(app.on_click(4), Action::Jump("%10".into()), "plain pane");
        assert_eq!(app.selected, Some(2));
        assert_eq!(app.on_click(99), Action::None);
    }

    #[test]
    fn focused_selection_stays_on_its_row_as_windows_come_and_go() {
        let mut app = app_in("@me1");
        app.on_focus(true);
        app.selected = Some(11);
        let mut s = status();
        s.windows.push(win("aa", 1, "new", &[]));
        app.set_status(s);
        assert_eq!(app.selected, Some(12));
        assert_eq!(app.on_key(enter()), Action::Jump("%1".into()));
        app.set_status(status());
        assert_eq!(app.selected, Some(11), "and back");
        let mut s = status();
        s.panes.retain(|r| r.pane != "%1");
        s.windows[4].panes.clear();
        app.set_status(s);
        assert_eq!(app.selected, Some(11), "a vanished row keeps the index");
    }

    #[test]
    fn focused_selection_on_an_unplaced_agent_stays_off_its_header() {
        let mut s = status();
        let mut unplaced = rec("%8", State::Idle, "zz", 4, true);
        unplaced.window_id = None;
        unplaced.window = None;
        s.panes.push(unplaced);
        let mut app = app_in("@me1");
        app.set_status(s.clone());
        app.on_focus(true);
        app.selected = Some(app.items().len() - 1);
        app.set_status(s);
        assert!(
            matches!(&app.items()[app.selected.unwrap()], Item::Agent(r) if r.pane == "%8"),
            "{:?}",
            app.selected
        );
    }

    #[test]
    fn clicking_the_place_of_an_unplaced_agent_selects_the_agent() {
        let mut s = status();
        let mut unplaced = rec("%8", State::Idle, "zz", 4, true);
        unplaced.window_id = None;
        unplaced.window = None;
        s.panes.push(unplaced);
        let mut app = app_in("@me1");
        app.set_status(s);
        let out = app.lines();
        assert_eq!(out[out.len() - 3], window("%8", "?"));
        assert_eq!(app.on_click(out.len() - 1), Action::Jump("%8".into()));
        assert!(
            matches!(&app.items()[app.selected.unwrap()], Item::Agent(r) if r.pane == "%8"),
            "{:?}",
            app.selected
        );
    }

    #[test]
    fn line_of_maps_a_selection_to_its_line() {
        let out = all(&status());
        assert_eq!(line_of(&out, 0), Some(1));
        assert_eq!(line_of(&out, 1), Some(2));
        assert_eq!(line_of(&out, 2), Some(4), "a plain pane");
        assert_eq!(line_of(&out, 99), None);
    }

    #[test]
    fn scrolling_keeps_headers_and_place_lines_in_view() {
        let out = all(&status());
        assert_eq!(scroll(&out, 15, 16, 5), 14, "a window shows its session");
        assert_eq!(
            scroll(&out, 2, 5, 5),
            0,
            "a first agent shows its window and session"
        );
        assert_eq!(scroll(&out, 13, 14, 5), 12, "a first pane shows its window");
        assert_eq!(
            scroll(&out, 10, 11, 5),
            10,
            "a later agent needs only itself"
        );
        assert_eq!(scroll(&out, 16, 0, 5), 13, "down: the place line too");
        assert_eq!(scroll(&out, 4, 2, 5), 2, "already in view");
        assert_eq!(scroll(&out, 4, 2, 0), 2, "no height");
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
