use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::protocol::{
    GitStatus, PaneRecord, PaneSummary, Run, RunState, State, Status, WindowInfo,
};
use crate::text::fmt_age;

/// What the sidebar lists; `f` switches between the two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Every session, window, and pane.
    #[default]
    All,
    /// Only agents, most urgent state first.
    Status,
}

impl Mode {
    pub fn next(self) -> Mode {
        match self {
            Mode::All => Mode::Status,
            Mode::Status => Mode::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::All => "all",
            Mode::Status => "status",
        }
    }
}

/// Where a state sorts in the status view: what needs you comes first.
fn rank(state: State) -> u8 {
    match state {
        State::Blocked => 0,
        State::Asking => 1,
        State::Failed => 2,
        State::Done => 3,
        State::Working => 4,
        State::Idle => 5,
        State::Unknown => 6,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub pane: String,
    pub state: State,
    /// Where the agent is, as `place` puts it, else the agent's name.
    pub label: String,
    pub age: String,
    pub window_id: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Session {
        name: String,
        /// While folded, its agents by state and its running or finished panes, in
        /// `mark_rank` order; `None` while open.
        folded: Option<Vec<(Mark, usize)>>,
    },
    /// A window header; `id` is what jumping to it targets.
    Window {
        id: String,
        label: String,
    },
    Agent(Row),
    /// Line under an agent in the status view: its session and window.
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
        /// What its foreground process is doing; `None` at a shell prompt.
        run: Option<RunState>,
        /// Time in `run`, for running and finished processes; empty otherwise.
        age: String,
    },
}

/// What a folded session header counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Agent(State),
    Run(RunState),
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

/// Shapes for plain panes, apart from every agent `symbol`.
pub fn run_symbol(run: RunState) -> char {
    match run {
        RunState::Running => '\u{25b6}',
        RunState::Finished => '\u{25a0}',
        RunState::Interactive => '\u{25b7}',
        // `pane_run` drops these, so none is drawn.
        RunState::Unknown => ' ',
    }
}

/// What a folded header shows for `mark`.
pub fn mark_symbol(mark: Mark) -> char {
    match mark {
        Mark::Agent(s) => symbol(s),
        Mark::Run(r) => run_symbol(r),
    }
}

/// Agents by `rank`, then unseen finishes, then what still runs.
fn mark_rank(mark: Mark) -> u8 {
    match mark {
        Mark::Agent(s) => rank(s),
        Mark::Run(RunState::Finished) => 7,
        Mark::Run(_) => 8,
    }
}

/// The pane's run when it is one you wait on: running, or finished unseen. These show their
/// age and count in folded headers.
fn counted_run(p: &PaneSummary) -> Option<Run> {
    p.run
        .filter(|r| matches!(r.state, RunState::Running | RunState::Finished))
}

/// The pane's run, if this build knows how to draw it.
fn pane_run(p: &PaneSummary) -> Option<RunState> {
    p.run.map(|r| r.state).filter(|s| *s != RunState::Unknown)
}

/// The window header an agent sits under; one the daemon has not placed in a known window
/// still gets a header of its own.
fn window_key(r: &PaneRecord) -> String {
    match (&r.window_id, &r.session, r.window) {
        (Some(id), _, _) => id.clone(),
        // `=` makes tmux match the session name exactly, even one like "3".
        (None, Some(session), Some(window)) => format!("={session}:{window}"),
        _ => r.pane.clone(),
    }
}

/// How a line is drawn: dimmed when nothing runs in it, at full color when something does,
/// and bold for the pane you are in with its window and session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Dim,
    Active,
    Highlighted,
}

/// The panes that run something (any agent, or a process running, open, or newly finished),
/// with their windows and sessions.
#[derive(Debug, Default)]
struct Running {
    panes: HashSet<String>,
    windows: HashSet<String>,
    sessions: HashSet<String>,
}

impl Running {
    fn of(status: &Status) -> Running {
        let mut running = Running::default();
        for r in &status.panes {
            running.add(&r.pane, &window_key(r), r.session.as_deref());
        }
        for w in &status.windows {
            for p in w.panes.iter().filter(|p| pane_run(p).is_some()) {
                running.add(&p.id, &w.id, Some(&w.session));
            }
        }
        running
    }

    fn add(&mut self, pane: &str, window: &str, session: Option<&str>) {
        self.panes.insert(pane.to_owned());
        self.windows.insert(window.to_owned());
        self.sessions.extend(session.map(str::to_owned));
    }

    fn line(&self, line: &Line) -> bool {
        let set = match line.kind() {
            Kind::Session => &self.sessions,
            Kind::Window => &self.windows,
            Kind::Pane => &self.panes,
        };
        set.contains(line.target())
    }
}

/// The tone of each line, from `App::tones`.
#[derive(Debug, Default)]
pub struct Tones {
    running: Running,
    pane: Option<String>,
    window: Option<String>,
    session: Option<String>,
}

impl Tones {
    pub fn of(&self, line: &Line) -> Tone {
        let focused = match line.kind() {
            Kind::Session => &self.session,
            Kind::Window => &self.window,
            Kind::Pane => &self.pane,
        };
        if focused.as_deref() == Some(line.target()) {
            Tone::Highlighted
        } else if self.running.line(line) {
            Tone::Active
        } else {
            Tone::Dim
        }
    }
}

/// A clickable line: what a click jumps to or folds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A session header, which a click folds or unfolds.
    Session(String),
    Window(String),
    Agent(Row),
    Pane {
        id: String,
        window: String,
        active: bool,
    },
}

impl Item {
    pub fn target(&self) -> &str {
        match self {
            Item::Session(id) | Item::Window(id) | Item::Pane { id, .. } => id,
            Item::Agent(row) => &row.pane,
        }
    }

    fn in_window(&self, window: &str) -> bool {
        match self {
            Item::Session(_) | Item::Window(_) => false,
            Item::Agent(r) => r.window_id.as_deref() == Some(window),
            Item::Pane { window: w, .. } => w == window,
        }
    }

    fn active(&self) -> bool {
        match self {
            Item::Session(_) | Item::Window(_) => false,
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
    text_with_age(&row.label, &row.age, width)
}

/// `label`, then `age` right-aligned to `width`; just `label`, cut to fit, without an age.
pub fn text_with_age(label: &str, age: &str, width: usize) -> String {
    if age.is_empty() {
        return truncate(label, width);
    }
    let room = width.saturating_sub(age.chars().count() + 1);
    let label = truncate(label, room);
    format!("{label:<room$} {age}")
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

/// The sidebar's lines in `mode`. `home` shortens paths (see `short_path`).
pub fn lines(status: &Status, mode: Mode, home: &str) -> Vec<Line> {
    match mode {
        Mode::All => tree_lines(status, home),
        Mode::Status => status_lines(status, home),
    }
}

/// Lists every window under its session, and each pane under its window, as an agent row when
/// an agent runs there. Sessions sort by name, numbers in numeric order, so rows don't jump as
/// focus moves. A collapsed session is just its header, with its agents counted by state.
fn tree_lines(status: &Status, home: &str) -> Vec<Line> {
    let collapsed =
        |session: Option<&str>| session.is_some_and(|s| status.collapsed.iter().any(|c| c == s));
    let mut windows: Vec<Window> = status.windows.iter().map(window_of).collect();
    for r in &status.panes {
        let id = window_key(r);
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
        .map(|w| {
            let entries: Vec<Placed> = entries(&w)
                .into_iter()
                .map(|e| {
                    let place = entry_place(&e, home);
                    (e, place)
                })
                .collect();
            (w, entries)
        })
        .collect();
    windows.sort_by(|(a, _), (b, _)| {
        let session = match (a.session, b.session) {
            (Some(x), Some(y)) => natural_cmp(x, y),
            (x, y) => x.is_none().cmp(&y.is_none()),
        };
        session.then_with(|| (a.index.is_none(), a.index).cmp(&(b.index.is_none(), b.index)))
    });
    // A folded session's header counts the agents and the running or finished panes in all
    // its windows.
    let mut marks: HashMap<Option<&str>, Vec<Mark>> = HashMap::new();
    for (w, entries) in &windows {
        for (e, _) in entries {
            let mark = match e {
                Entry::Agent(r, _) => Mark::Agent(r.state),
                Entry::Pane(p) => match counted_run(p) {
                    Some(run) => Mark::Run(run.state),
                    None => continue,
                },
            };
            marks.entry(w.session).or_default().push(mark);
        }
    }
    let mut out = Vec::new();
    let mut current: Option<Option<&str>> = None;
    let mut open = true;
    for (w, entries) in windows {
        if current != Some(w.session) {
            current = Some(w.session);
            open = !collapsed(w.session);
            let folded = (!open).then(|| {
                let mut counts: Vec<(Mark, usize)> = Vec::new();
                for &m in marks.get(&w.session).into_iter().flatten() {
                    match counts.iter_mut().find(|(t, _)| *t == m) {
                        Some((_, n)) => *n += 1,
                        None => counts.push((m, 1)),
                    }
                }
                counts.sort_by_key(|&(m, _)| mark_rank(m));
                counts
            });
            out.push(Line::Session {
                name: w.session.unwrap_or("?").to_owned(),
                folded,
            });
        }
        if !open {
            continue;
        }
        out.push(Line::Window {
            id: w.id.clone(),
            label: w.label.clone(),
        });
        for (e, place) in entries {
            let r = match e {
                Entry::Agent(r, _) => r,
                Entry::Pane(p) => {
                    let run = pane_run(p);
                    let age = counted_run(p).map_or_else(String::new, |r| {
                        fmt_age(status.now_ms.saturating_sub(r.since_ms))
                    });
                    out.push(Line::Pane {
                        id: p.id.clone(),
                        place: place.unwrap_or_default(),
                        window: w.id.clone(),
                        active: p.active,
                        run,
                        age,
                    });
                    continue;
                }
            };
            out.push(Line::Agent(row(r, place, status.now_ms)));
        }
    }
    out
}

/// Lists only agents, without headers: by `rank`, then longest in its state first. Under each
/// agent, its session and window.
fn status_lines(status: &Status, home: &str) -> Vec<Line> {
    let mut agents: Vec<(&PaneRecord, String, Option<String>)> = status
        .panes
        .iter()
        .map(|r| {
            let found = status
                .windows
                .iter()
                .find_map(|w| Some((w, w.panes.iter().find(|p| p.id == r.pane)?)));
            let location = match found {
                Some((w, _)) => format!(
                    "{} {}",
                    w.session,
                    window_label(Some(w.index), Some(&w.name))
                ),
                None => format!("{} {}", r.session.as_deref().unwrap_or("?"), label(r)),
            };
            let place = entry_place(&Entry::Agent(r, found.map(|(_, p)| p)), home);
            (r, location, place)
        })
        .collect();
    agents.sort_by_key(|(r, ..)| (rank(r.state), r.since_ms, pane_number(&r.pane)));
    let mut out = Vec::new();
    for (r, location, place) in agents {
        out.push(Line::Agent(row(r, place, status.now_ms)));
        out.push(Line::Place {
            pane: r.pane.clone(),
            text: location,
        });
    }
    out
}

fn row(r: &PaneRecord, place: Option<String>, now_ms: u64) -> Row {
    Row {
        pane: r.pane.clone(),
        state: r.state,
        label: place.unwrap_or_else(|| r.agent.name().to_owned()),
        age: fmt_age(now_ms.saturating_sub(r.since_ms)),
        window_id: r.window_id.clone(),
        active: r.active,
    }
}

/// `%12` is 12; anything else sorts last.
fn pane_number(pane: &str) -> u32 {
    pane.trim_start_matches('%')
        .parse::<u32>()
        .unwrap_or(u32::MAX)
}

/// What a line stands for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Session,
    Window,
    Pane,
}

impl Line {
    fn kind(&self) -> Kind {
        match self {
            Line::Session { .. } => Kind::Session,
            Line::Window { .. } => Kind::Window,
            Line::Agent(_) | Line::Pane { .. } | Line::Place { .. } => Kind::Pane,
        }
    }

    /// What this line stands for: a session name, a window id, or a pane id.
    pub fn target(&self) -> &str {
        match self {
            Line::Session { name, .. } => name,
            Line::Window { id, .. } | Line::Pane { id, .. } => id,
            Line::Agent(r) => &r.pane,
            Line::Place { pane, .. } => pane,
        }
    }
}

/// Orders names with runs of digits compared as numbers, so `6267` comes before `11212`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (chunks(a), chunks(b));
    loop {
        let (p, q) = match (x.next(), y.next()) {
            // Equal chunks can still differ in leading zeros; keep the order total.
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => (p, q),
        };
        let digits = |s: &str| s.starts_with(|c: char| c.is_ascii_digit());
        let order = if digits(p) && digits(q) {
            // By length, not by parsing, so no run of digits is too long.
            let (p, q) = (p.trim_start_matches('0'), q.trim_start_matches('0'));
            p.len().cmp(&q.len()).then_with(|| p.cmp(q))
        } else {
            p.cmp(q)
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

/// Splits `s` into alternating runs of ASCII digits and everything else.
fn chunks(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let digit = rest.chars().next()?.is_ascii_digit();
        let end = rest
            .find(|c: char| c.is_ascii_digit() != digit)
            .unwrap_or(rest.len());
        let (chunk, tail) = rest.split_at(end);
        rest = tail;
        Some(chunk)
    })
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
    rest.sort_by_key(|r| pane_number(&r.pane));
    out.extend(rest.into_iter().map(|r| Entry::Agent(r, None)));
    out
}

fn selectable(line: &Line) -> bool {
    matches!(
        line,
        Line::Session { .. } | Line::Window { .. } | Line::Agent(_) | Line::Pane { .. }
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
            Line::Session { name, .. } => Some(Item::Session(name.clone())),
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
    /// Fold (`collapsed`) or unfold a session in every sidebar.
    Collapse {
        session: String,
        collapsed: bool,
    },
}

#[derive(Debug, Default)]
pub struct App {
    pub status: Option<Status>,
    /// The window this sidebar lives in; its active pane's row is highlighted.
    pub own_window: Option<String>,
    pub mode: Mode,
    /// `$HOME`, which shortened paths start from.
    pub home: String,
}

impl App {
    pub fn lines(&self) -> Vec<Line> {
        match &self.status {
            Some(s) => lines(s, self.mode, &self.home),
            None => Vec::new(),
        }
    }

    /// The session this sidebar's window is in.
    pub fn own_session(&self) -> Option<&str> {
        let window = self.own_window.as_deref()?;
        let status = self.status.as_ref()?;
        let info = status.windows.iter().find(|w| w.id == window)?;
        Some(&info.session)
    }

    /// The line in `lines` for the pane you are in: this sidebar's window's active pane, else
    /// its first agent, else the window itself, else its folded session's header.
    pub fn highlighted_line(&self, lines: &[Line]) -> Option<usize> {
        let window = self.own_window.as_deref()?;
        let items = items(lines);
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
            })
            .or_else(|| {
                let session = self.own_session()?;
                items
                    .iter()
                    .find(|i| matches!(i, Item::Session(s) if s == session))
            })?;
        line_of(lines, items.iter().position(|i| i == pick)?)
    }

    /// The tones of the lines: the pane you are in, this sidebar's window's active pane, stands
    /// out with its window and session. No other line stands in for a pane that has none.
    pub fn tones(&self) -> Tones {
        let window = self.own_window.as_deref();
        let pane = self.status.as_ref().and_then(|s| {
            let summary = s
                .windows
                .iter()
                .filter(|w| Some(w.id.as_str()) == window)
                .flat_map(|w| &w.panes)
                .find(|p| p.active)
                .map(|p| p.id.clone());
            summary.or_else(|| {
                let agent = s
                    .panes
                    .iter()
                    .find(|r| r.active && r.window_id.as_deref() == window);
                agent.map(|r| r.pane.clone())
            })
        });
        Tones {
            running: self.status.as_ref().map(Running::of).unwrap_or_default(),
            pane,
            window: self.own_window.clone(),
            session: self.own_session().map(str::to_owned),
        }
    }

    pub fn set_status(&mut self, status: Status) {
        self.status = Some(status);
    }

    pub fn set_location(&mut self, window: String) {
        self.own_window = Some(window);
    }

    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.next();
    }

    /// Clicking line `index` (0-based, as rendered): a session header folds or unfolds, and
    /// anything else jumps to its window or pane; a place line acts for its agent.
    pub fn on_click(&self, index: usize) -> Action {
        let lines = self.lines();
        if index >= lines.len() {
            return Action::None;
        }
        let Some(line) = lines[..=index]
            .iter()
            .rposition(|l| !matches!(l, Line::Place { .. }))
            .filter(|&i| selectable(&lines[i]))
        else {
            return Action::None;
        };
        if let Line::Session { name, folded } = &lines[line] {
            return Action::Collapse {
                session: name.clone(),
                collapsed: folded.is_none(),
            };
        }
        let n = lines[..line].iter().filter(|l| selectable(l)).count();
        match items(&lines).into_iter().nth(n) {
            Some(item) => Action::Jump(item.target().to_owned()),
            None => Action::None,
        }
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

    /// Lines, each also its item number:
    /// 0 me | 1 1:w1 | 2 %3 | 3 %10 zsh | 4 2:notes | 5 %11 zsh | 6 3:w3 | 7 %2 | 8 %4 |
    /// 9 9:w9 | 10 %12 vim | 11 ops | 12 2:w2 | 13 %1 | 14 web | 15 1:w1 | 16 %5 | 17 zz |
    /// 18 1:logs | 19 %13 tail
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
            collapsed: vec![],
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
                    run: None,
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
        Line::Session {
            name: name.into(),
            folded: None,
        }
    }

    /// A folded session header with these agent counts.
    fn folded(name: &str, counts: &[(State, usize)]) -> Line {
        let marks: Vec<(Mark, usize)> = counts.iter().map(|&(s, n)| (Mark::Agent(s), n)).collect();
        folded_marks(name, &marks)
    }

    fn folded_marks(name: &str, counts: &[(Mark, usize)]) -> Line {
        Line::Session {
            name: name.into(),
            folded: Some(counts.to_vec()),
        }
    }

    /// The fixture with plain panes %10 running since 120s, %11 finished since 65s, and %12
    /// (vim) interactive since 0.
    fn status_with_runs() -> Status {
        let mut s = status();
        for p in s.windows.iter_mut().flat_map(|w| &mut w.panes) {
            let (state, since_ms) = match p.id.as_str() {
                "%10" => (RunState::Running, 120_000),
                "%11" => (RunState::Finished, 65_000),
                "%12" => (RunState::Interactive, 0),
                _ => continue,
            };
            p.run = Some(Run { state, since_ms });
        }
        s
    }

    /// `s` with these sessions marked collapsed.
    fn collapse(mut s: Status, sessions: &[&str]) -> Status {
        s.collapsed = sessions.iter().map(|&s| s.to_owned()).collect();
        s
    }

    /// The fixture with these sessions marked collapsed.
    fn status_collapsed(sessions: &[&str]) -> Status {
        collapse(status(), sessions)
    }

    /// A plain pane line, in `~/src/<command>` like the fixture's.
    fn pane(id: &str, command: &str, window: &str, active: bool) -> Line {
        Line::Pane {
            id: id.into(),
            place: format!("/s/{command}"),
            window: window.into(),
            active,
            run: None,
            age: String::new(),
        }
    }

    fn all(status: &Status) -> Vec<Line> {
        lines(status, Mode::All, HOME)
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
        let runs = [RunState::Running, RunState::Finished, RunState::Interactive].map(run_symbol);
        assert_eq!(runs, ['\u{25b6}', '\u{25a0}', '\u{25b7}']);
        let mut every: Vec<char> = all.into_iter().chain(runs).collect();
        every.sort_unstable();
        every.dedup();
        assert_eq!(every.len(), 10, "processes never share an agent's shape");
    }

    #[test]
    fn plain_pane_rows_carry_their_run_and_age() {
        let out = all(&status_with_runs());
        let run_of = |id: &str| {
            out.iter().find_map(|l| match l {
                Line::Pane {
                    id: i, run, age, ..
                } if i == id => Some((*run, age.clone())),
                _ => None,
            })
        };
        assert_eq!(
            run_of("%10"),
            Some((Some(RunState::Running), fmt_age(5_000)))
        );
        assert_eq!(
            run_of("%11"),
            Some((Some(RunState::Finished), fmt_age(60_000)))
        );
        assert_eq!(
            run_of("%12"),
            Some((Some(RunState::Interactive), String::new())),
            "an editor's age says nothing"
        );
        assert_eq!(run_of("%13"), Some((None, String::new())));
    }

    #[test]
    fn unknown_run_states_draw_as_no_run() {
        let mut s = status();
        s.windows[0].panes[1].run = Some(Run {
            state: RunState::Unknown,
            since_ms: 0,
        });
        assert_eq!(all(&s), all(&status()));
    }

    #[test]
    fn folded_counts_include_running_and_finished_panes() {
        let out = all(&collapse(status_with_runs(), &["me"]));
        assert_eq!(
            out[0],
            folded_marks(
                "me",
                &[
                    (Mark::Agent(State::Done), 1),
                    (Mark::Agent(State::Working), 1),
                    (Mark::Agent(State::Idle), 1),
                    (Mark::Run(RunState::Finished), 1),
                    (Mark::Run(RunState::Running), 1),
                ]
            ),
            "agents first, then finished before running; interactive isn't counted"
        );
    }

    #[test]
    fn the_status_view_ignores_runs() {
        assert_eq!(
            lines(&status_with_runs(), Mode::Status, HOME),
            lines(&status(), Mode::Status, HOME)
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
        assert_eq!(row.label, "br-%3", "where it is, beside its symbol");
        assert_eq!(row.age, "2m");
        assert!(
            !out.iter().any(|l| matches!(l, Line::Place { .. })),
            "no separate place line: {out:?}"
        );
        let web = out.iter().position(|l| *l == session("web")).unwrap();
        assert!(
            matches!(&out[web + 2], Line::Agent(r) if r.pane == "%5" && r.label == "/s/claude"),
            "outside git, the directory: {out:?}"
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
                Line::Agent(r) => Some(r.label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(places, ["br-%1", "/x/y"], "{out:?}");
    }

    #[test]
    fn agent_nowhere_known_shows_its_agent_name() {
        let mut s = status();
        s.windows.clear();
        s.panes.truncate(1);
        s.panes[0].cwd = None;
        for mode in [Mode::All, Mode::Status] {
            let out = lines(&s, mode, HOME);
            assert!(
                out.iter()
                    .any(|l| matches!(l, Line::Agent(r) if r.label == "claude")),
                "{mode:?}: {out:?}"
            );
        }
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
        assert_eq!(out[4], window("@me2", "2:notes"));
        assert_eq!(out[6], window("@me3", "3:w3"));
        assert_eq!(out[9], window("@me9", "9:w9"));
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
        assert_eq!(all(&status())[10], pane("%12", "vim", "@me9", true));
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
        assert_eq!(out[web + 3], window("=web:4", "4:w4"), "a tmux target");
        assert!(
            matches!(&out[web + 5], Line::Agent(r) if r.pane == "%9"),
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
    fn status_view_lists_only_agents_with_where_they_are() {
        let out: Vec<Line> = lines(&status(), Mode::Status, HOME)
            .into_iter()
            .filter(|l| {
                matches!(l, Line::Agent(r) if r.pane == "%1")
                    || matches!(l, Line::Place { pane, .. } if pane == "%1")
            })
            .collect();
        assert_eq!(
            out,
            [
                Line::Agent(Row {
                    pane: "%1".into(),
                    state: State::Blocked,
                    label: "br-%1".into(),
                    age: "2m".into(),
                    window_id: Some("@ops2".into()),
                    active: true,
                }),
                Line::Place {
                    pane: "%1".into(),
                    text: "ops 2:w2".into()
                },
            ]
        );
        let out = lines(&status(), Mode::Status, HOME);
        assert!(
            out.iter()
                .all(|l| matches!(l, Line::Agent(_) | Line::Place { .. })),
            "no headers or plain panes: {out:?}"
        );
        assert_eq!(panes(&out).len(), 5);
    }

    #[test]
    fn status_view_orders_by_priority_then_longest_in_state() {
        let mut s = status();
        for (pane, state, session) in [("%6", State::Failed, "web"), ("%7", State::Asking, "zz")] {
            let mut unplaced = rec(pane, state, session, 4, false);
            unplaced.window_id = None;
            s.panes.push(unplaced);
        }
        let out = lines(&s, Mode::Status, HOME);
        assert_eq!(panes(&out), ["%1", "%7", "%6", "%2", "%3", "%4", "%5"]);
        let p7 = out
            .iter()
            .position(|l| matches!(l, Line::Agent(r) if r.pane == "%7"))
            .unwrap();
        assert!(
            matches!(&out[p7], Line::Agent(r) if r.label == "br-%7"),
            "outside known windows, the agent's own branch: {out:?}"
        );
        assert_eq!(
            out[p7 + 1..p7 + 2],
            [Line::Place {
                pane: "%7".into(),
                text: "zz 4:w4".into()
            }],
            "and its own session and window"
        );
        s.panes
            .iter_mut()
            .find(|r| r.pane == "%4")
            .unwrap()
            .since_ms = 1_000;
        let out = lines(&s, Mode::Status, HOME);
        assert_eq!(panes(&out)[5..], ["%5", "%4"], "idle longer comes first");
    }

    #[test]
    fn sessions_sort_by_name() {
        let out = all(&status());
        let sessions: Vec<&str> = out
            .iter()
            .filter_map(|l| match l {
                Line::Session { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(sessions, ["me", "ops", "web", "zz"]);
    }

    #[test]
    fn sessions_sort_naturally() {
        let mut s = status();
        s.windows
            .push(win("11212", 1, "w1", &[("%20", "zsh", true)]));
        s.windows
            .push(win("6267", 1, "w1", &[("%21", "zsh", true)]));
        let sessions: Vec<String> = all(&s)
            .into_iter()
            .filter_map(|l| match l {
                Line::Session { name, .. } => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(sessions, ["6267", "11212", "me", "ops", "web", "zz"]);
    }

    #[test]
    fn natural_cmp_orders_digit_runs_numerically() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("6267", "11212"), Less);
        assert_eq!(natural_cmp("web2", "web10"), Less);
        assert_eq!(natural_cmp("x1y10", "x1y2"), Greater);
        assert_eq!(natural_cmp("a", "b"), Less);
        assert_eq!(natural_cmp("me", "me"), Equal);
        assert_ne!(natural_cmp("007", "7"), Equal, "order stays total");
        assert_eq!(natural_cmp("007", "8"), Less, "leading zeros don't count");
        let long = "9".repeat(40);
        assert_eq!(natural_cmp(&long, &format!("1{long}")), Less);
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
        assert_eq!(agent_text(&row, 10), "br-%3   2m");
        row.label = "1:a-very-long-name".into();
        assert_eq!(agent_text(&row, 12), "1:a-very\u{2026} 2m");
    }

    #[test]
    fn mode_toggles_between_all_and_status() {
        let labels: Vec<&str> = [Mode::All, Mode::All.next(), Mode::All.next().next()]
            .map(Mode::label)
            .into();
        assert_eq!(labels, ["all", "status", "all"]);
    }

    /// The line the sidebar highlights in `app`.
    fn highlight(app: &App) -> Option<usize> {
        app.highlighted_line(&app.lines())
    }

    #[test]
    fn highlight_follows_own_window() {
        assert_eq!(highlight(&app_in("@me1")), Some(2));
        assert_eq!(
            highlight(&app_in("@me3")),
            Some(7),
            "the active pane of the window"
        );
        assert_eq!(highlight(&app_in("@me2")), Some(5), "an active plain pane");
        let mut app = app_in("@me9");
        let mut s = status();
        s.windows[3].panes.clear();
        app.set_status(s);
        assert_eq!(highlight(&app), Some(9), "no panes: the window");
        assert_eq!(highlight(&app_in("@gone")), None, "unknown window");
    }

    #[test]
    fn own_session_is_the_own_windows_session() {
        assert_eq!(app_in("@me1").own_session(), Some("me"));
        assert_eq!(app_in("@gone").own_session(), None, "unknown window");
        let mut app = App::default();
        app.set_status(status());
        assert_eq!(app.own_session(), None, "no window yet");
    }

    #[test]
    fn highlight_tracks_status_updates() {
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
        assert_eq!(highlight(&app), Some(11));
    }

    #[test]
    fn toggling_the_view_moves_the_highlight_to_the_same_pane() {
        let mut app = app_in("@me3");
        app.toggle_mode();
        assert_eq!(app.mode, Mode::Status);
        assert!(
            matches!(&app.lines()[highlight(&app).unwrap()], Line::Agent(r) if r.pane == "%2"),
            "{:?}",
            highlight(&app)
        );
        app.set_location("@me9".into());
        assert_eq!(highlight(&app), None, "a window without agents has no row");
        app.toggle_mode();
        assert_eq!(app.mode, Mode::All);
    }

    #[test]
    fn status_view_click_on_any_detail_line_jumps_to_its_agent() {
        let mut app = app_in("@me1");
        app.mode = Mode::Status;
        assert_eq!(app.on_click(3), Action::Jump("%2".into()), "location line");
        assert_eq!(app.on_click(1), Action::Jump("%1".into()), "location line");
        assert_eq!(app.on_click(99), Action::None);
    }

    #[test]
    fn click_on_window_agent_or_pane_jumps() {
        let app = app_in("@me9");
        assert_eq!(app.on_click(7), Action::Jump("%2".into()));
        assert_eq!(
            app.on_click(8),
            Action::Jump("%4".into()),
            "the next agent, with no place line between"
        );
        assert_eq!(
            app.on_click(4),
            Action::Jump("@me2".into()),
            "window header"
        );
        assert_eq!(app.on_click(3), Action::Jump("%10".into()), "plain pane");
        assert_eq!(app.on_click(99), Action::None);
    }

    #[test]
    fn click_on_session_header_folds_and_unfolds_it() {
        let mut app = app_in("@me9");
        assert_eq!(
            app.on_click(11),
            Action::Collapse {
                session: "ops".into(),
                collapsed: true
            }
        );
        app.set_status(status_collapsed(&["ops"]));
        let out = app.lines();
        assert_eq!(out[11], folded("ops", &[(State::Blocked, 1)]));
        assert_eq!(
            app.on_click(11),
            Action::Collapse {
                session: "ops".into(),
                collapsed: false
            }
        );
    }

    #[test]
    fn clicking_an_unplaced_agent_jumps_to_it_not_its_header() {
        let mut s = status();
        let mut unplaced = rec("%8", State::Idle, "zz", 4, true);
        unplaced.window_id = None;
        unplaced.window = None;
        s.panes.push(unplaced);
        let mut app = app_in("@me1");
        app.set_status(s);
        let out = app.lines();
        assert_eq!(out[out.len() - 2], window("%8", "?"));
        assert_eq!(app.on_click(out.len() - 1), Action::Jump("%8".into()));
    }

    #[test]
    fn line_of_maps_a_selection_to_its_line() {
        let out = all(&status());
        assert_eq!(line_of(&out, 0), Some(0), "a session header");
        assert_eq!(line_of(&out, 1), Some(1));
        assert_eq!(line_of(&out, 2), Some(2));
        assert_eq!(line_of(&out, 3), Some(3), "a plain pane");
        assert_eq!(line_of(&out, 99), None);
        let out = lines(&status(), Mode::Status, HOME);
        assert_eq!(
            line_of(&out, 1),
            Some(2),
            "past the first agent's location line"
        );
    }

    #[test]
    fn folded_session_is_its_header_with_agents_by_state() {
        let out = all(&status_collapsed(&["me", "zz"]));
        assert_eq!(
            out[0],
            folded(
                "me",
                &[(State::Done, 1), (State::Working, 1), (State::Idle, 1)]
            ),
            "most urgent first"
        );
        assert_eq!(out[1], session("ops"));
        assert_eq!(out.last(), Some(&folded("zz", &[])), "no agents: no counts");
        assert_eq!(out.len(), all(&status()).len() - 10 - 2);
        assert_eq!(panes(&out), ["%1", "%5"]);
    }

    #[test]
    fn the_status_view_ignores_folding() {
        let s = status_collapsed(&["me", "ops"]);
        assert_eq!(
            lines(&s, Mode::Status, HOME),
            lines(&status(), Mode::Status, HOME)
        );
        let mut app = app_in("@me3");
        app.set_status(s);
        app.toggle_mode();
        assert_eq!(
            app.on_click(0),
            Action::Jump("%1".into()),
            "no sessions to fold"
        );
    }

    #[test]
    fn highlight_in_a_folded_session_is_its_header() {
        let mut app = app_in("@me3");
        app.set_status(status_collapsed(&["me"]));
        assert_eq!(highlight(&app), Some(0));
        let mut app = app_in("@ops2");
        app.set_status(status_collapsed(&["ops"]));
        assert!(matches!(
            &app.lines()[highlight(&app).unwrap()],
            Line::Session { name, .. } if name == "ops"
        ));
    }

    #[test]
    fn scrolling_keeps_headers_and_place_lines_in_view() {
        let out = all(&status());
        assert_eq!(scroll(&out, 12, 13, 5), 11, "a window shows its session");
        assert_eq!(
            scroll(&out, 2, 5, 5),
            0,
            "a first agent shows its window and session"
        );
        assert_eq!(scroll(&out, 10, 11, 5), 9, "a first pane shows its window");
        assert_eq!(scroll(&out, 8, 9, 5), 8, "a later agent needs only itself");
        assert_eq!(scroll(&out, 13, 0, 5), 9, "down: the agent at the bottom");
        let status_view = lines(&status(), Mode::Status, HOME);
        assert_eq!(
            scroll(&status_view, 4, 0, 5),
            1,
            "down: its location line too"
        );
        assert_eq!(scroll(&out, 4, 2, 5), 2, "already in view");
        assert_eq!(scroll(&out, 4, 2, 0), 2, "no height");
    }

    #[test]
    fn no_status_is_empty() {
        let app = App::default();
        assert!(app.lines().is_empty());
        assert_eq!(app.on_click(0), Action::None);
        assert_eq!(highlight(&app), None);
    }

    /// Each line of `app` with its tone, keyed by what it stands for.
    fn tones(app: &App) -> Vec<(String, Tone)> {
        let lines = app.lines();
        let tones = app.tones();
        lines
            .iter()
            .map(|l| (l.target().to_owned(), tones.of(l)))
            .collect()
    }

    fn with_tone(app: &App, tone: Tone) -> Vec<String> {
        tones(app)
            .into_iter()
            .filter_map(|(k, t)| (t == tone).then_some(k))
            .collect()
    }

    #[test]
    fn the_focused_pane_and_its_parents_are_highlighted() {
        let app = app_in("@me3");
        assert_eq!(with_tone(&app, Tone::Highlighted), ["me", "@me3", "%2"]);
    }

    #[test]
    fn agents_light_their_window_and_session_and_idle_shells_are_dim() {
        let app = app_in("@me3");
        assert_eq!(
            with_tone(&app, Tone::Active),
            [
                // An agent counts as running in any state, idle included.
                "@me1", "%3", "%4", "ops", "@ops2", "%1", "web", "@web1", "%5",
            ]
        );
        assert_eq!(
            with_tone(&app, Tone::Dim),
            [
                // Shells at their prompt and programs simul doesn't report, even in a
                // window's active pane.
                "%10", "@me2", "%11", "@me9", "%12", "zz", "@zz1", "%13",
            ]
        );
    }

    #[test]
    fn running_open_and_finished_processes_are_active_with_their_parents() {
        let mut app = app_in("@ops2");
        app.set_status(status_with_runs());
        let active = with_tone(&app, Tone::Active);
        for key in ["me", "@me1", "%10", "@me2", "%11", "@me9", "%12"] {
            assert!(active.contains(&key.to_owned()), "{key} in {active:?}");
        }
    }

    #[test]
    fn folded_sessions_take_the_tone_of_what_they_hold() {
        let mut app = app_in("@me3");
        app.set_status(status_collapsed(&["me", "ops", "zz"]));
        let tones = tones(&app);
        let tone = |name: &str| tones.iter().find(|(k, _)| k == name).unwrap().1;
        assert_eq!(tone("me"), Tone::Highlighted, "the own session");
        assert_eq!(tone("ops"), Tone::Active);
        assert_eq!(tone("zz"), Tone::Dim);
    }

    #[test]
    fn only_the_pane_you_are_in_is_highlighted_never_a_stand_in() {
        let mut s = status();
        // In 1:w1, focus moves from agent %3 to shell %10.
        for p in &mut s.windows[0].panes {
            p.active = p.id == "%10";
        }
        s.panes.iter_mut().find(|r| r.pane == "%3").unwrap().active = false;
        let mut app = app_in("@me1");
        app.set_status(s);
        assert_eq!(with_tone(&app, Tone::Highlighted), ["me", "@me1", "%10"]);
        app.toggle_mode();
        assert!(
            with_tone(&app, Tone::Highlighted).is_empty(),
            "the status view lists no shells, so no agent stands in for one"
        );
    }

    #[test]
    fn without_a_window_nothing_is_highlighted() {
        assert!(with_tone(&app_in("@gone"), Tone::Highlighted).is_empty());
    }

    #[test]
    fn an_agent_outside_known_windows_lights_its_own_header() {
        let mut s = status();
        let mut r = rec("%20", State::Idle, "x", 4, false);
        r.window_id = None;
        s.panes.push(r);
        let mut app = app_in("@me3");
        app.set_status(s);
        let active = with_tone(&app, Tone::Active);
        for key in ["x", "=x:4", "%20"] {
            assert!(active.contains(&key.to_owned()), "{key} in {active:?}");
        }
    }

    #[test]
    fn place_lines_take_their_agents_tone() {
        let mut app = app_in("@me3");
        app.toggle_mode();
        let lines = app.lines();
        let tones = app.tones();
        let place = |pane: &str| {
            tones.of(lines
                .iter()
                .find(|l| matches!(l, Line::Place { pane: p, .. } if p == pane))
                .unwrap())
        };
        assert_eq!(place("%2"), Tone::Highlighted);
        assert_eq!(place("%4"), Tone::Active);
    }
}
