pub mod control_mode;
pub mod notify;
pub mod runs;
pub mod scan;
pub mod server;
pub mod state;
pub mod update;
pub mod width;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::claude_metrics::{Turn, last_turn};
use crate::claude_sessions;
use crate::config::Config;
use crate::hook::{Agent, AgentEvent, Envelope};
use crate::kiro_sessions;
use crate::log;
use crate::protocol::{GitStatus, Identity, Status, WindowInfo, now_ms};
use crate::tmux::{self, Tmux, shell_quote, tmux_double_quote};
use runs::Runs;
use state::{PaneInfo, Snapshot, Store, Transition};

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(50);
const TICK: Duration = Duration::from_secs(1);
/// How long a pane directory's git status stays fresh while the sidebar shows it.
const GIT_REFRESH: Duration = Duration::from_secs(5);
/// Index in tmux hook arrays, so simul's entries never replace the user's.
const HOOK_INDEX: u32 = 7463;
const LAYOUT_HOOK: &str = "window-layout-changed";
/// Global tmux option holding the status-format[0] hidden while the sidebar is on.
const SAVED_STATUS: &str = "@simul-status-format";
/// Session option marking a session folded in the sidebar. It lives in tmux so it survives
/// daemon restarts, goes with its session, and follows renames.
const COLLAPSED: &str = "@simul-collapsed";
/// Pane option naming the build a sidebar runs (see `Identity::tag`), so a daemon that takes
/// over can restart sidebars still running an older one.
const SIDEBAR_EXE: &str = "@simul-sidebar-exe";
/// The right-hand part of tmux's default status-format[0]: status-right alone, without the
/// session name and window list the sidebar repeats.
const RIGHT_ONLY_STATUS: &str = "#[align=right range=right #{E:status-right-style}]#[push-default]#{T;=/#{status-right-length}:status-right}#[pop-default]#[norange default]";

/// tmux ends `show-options -v` output with one newline that isn't part of the value.
fn strip_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
    }
    s
}

pub enum Cmd {
    Hook(Envelope),
    List(oneshot::Sender<Status>),
    Subscribe(oneshot::Sender<(Status, broadcast::Receiver<Status>)>),
    Refresh,
    Toggle(oneshot::Sender<bool>),
    Resized,
    Collapse {
        session: String,
        collapsed: bool,
        reply: oneshot::Sender<bool>,
    },
    TmuxNotification {
        name: String,
    },
    Reconcile,
    /// A debounced `Resized`: reasserts sidebar widths alone.
    CheckWidths,
    Tick,
    Git {
        cwd: String,
        status: Option<GitStatus>,
    },
    Turn {
        pane: String,
        turn: Option<Turn>,
    },
    Identify(oneshot::Sender<Identity>),
    /// The plugin checkout's VERSION file changed.
    PluginChanged,
    /// The installer for `version` finished.
    Installed {
        version: String,
        result: Result<(), String>,
    },
    Shutdown(Exit),
}

/// Why the daemon stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    TmuxGone,
    /// Another build asked for the socket (see `update::replace`).
    Replaced,
}

pub struct Options {
    pub socket: PathBuf,
    pub tmux: Tmux,
    /// This binary, with symlinks resolved, so builds compare by where they really live.
    pub exe: PathBuf,
    /// This build, as read at startup (see `Identity::of`).
    pub identity: Identity,
    pub config: Config,
}

struct Daemon {
    opts: Options,
    store: Store,
    /// Foreground processes of panes without an agent.
    runs: Runs,
    /// When `runs` last read a snapshot.
    runs_checked: Option<Instant>,
    snapshot: Snapshot,
    /// The snapshot's windows as published, rebuilt at each reconcile.
    windows: Vec<WindowInfo>,
    recovered: bool,
    focus_events: bool,
    tx: mpsc::Sender<Cmd>,
    updates: broadcast::Sender<Status>,
    reconcile_pending: bool,
    widths_pending: bool,
    git_inflight: HashSet<String>,
    sidebar_on: bool,
    /// Windows seen at the last reconcile; only windows new since then get a sidebar.
    known_windows: HashSet<String>,
    /// Sessions folded in the sidebar, as read from `COLLAPSED` at the last reconcile.
    collapsed: BTreeSet<String>,
    notify_gate: notify::Gate,
    /// Git status by working directory, with when it was read; `None` outside a repo.
    git: HashMap<String, (Instant, Option<GitStatus>)>,
    /// Panes whose transcript is being read.
    turn_inflight: HashSet<String>,
    /// Where Claude Code keeps live session status; `None` without `$HOME`.
    sessions_dir: Option<PathBuf>,
    /// Where Kiro CLI keeps its session files; `None` without `$KIRO_HOME` or `$HOME`.
    kiro_sessions_dir: Option<PathBuf>,
    /// Last failure reading either sessions dir, so a persistent one is logged once.
    live_error: Option<String>,
    /// The plugin checkout this binary was installed into; `None` for other builds.
    plugin: Option<PathBuf>,
    /// The checkout's VERSION when last read, so repeated change events act once.
    plugin_version: Option<String>,
    /// An installer is running.
    updating: bool,
    /// Sidebar widths last asked of tmux (see `width::resizes`).
    width_requested: width::Requested,
}

pub async fn run(opts: Options) -> Result<()> {
    let Some(listener) = server::bind(&opts.socket)? else {
        log!("daemon already running at {}", opts.socket.display());
        return Ok(());
    };
    log!(
        "daemon started: socket {} tmux {}",
        opts.socket.display(),
        opts.tmux.socket().display()
    );
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(server::accept(listener, tx.clone()));
    tokio::spawn(control_mode::run(opts.tmux.clone(), tx.clone()));
    tokio::spawn(ticker(tx.clone()));
    tokio::spawn(ignore_sighup());
    let plugin = update::plugin_dir(&opts.exe);
    // Lives until the daemon exits.
    let _watcher = plugin
        .as_deref()
        .and_then(|p| match update::watch(p, tx.clone()) {
            Ok(w) => {
                log!("watching {} for updates", p.join("VERSION").display());
                Some(w)
            }
            Err(e) => {
                log!("{e:#}");
                None
            }
        });

    let (updates, _) = broadcast::channel(64);
    let mut d = Daemon {
        opts,
        store: Store::default(),
        runs: Runs::default(),
        runs_checked: None,
        snapshot: Snapshot::default(),
        windows: Vec::new(),
        recovered: false,
        focus_events: false,
        tx,
        updates,
        reconcile_pending: false,
        widths_pending: false,
        git_inflight: HashSet::new(),
        sidebar_on: false,
        known_windows: HashSet::new(),
        collapsed: BTreeSet::new(),
        notify_gate: notify::Gate::default(),
        git: HashMap::new(),
        turn_inflight: HashSet::new(),
        sessions_dir: claude_sessions::sessions_dir(crate::paths::env_var),
        kiro_sessions_dir: kiro_sessions::sessions_dir(crate::paths::env_var),
        live_error: None,
        plugin_version: plugin.as_deref().and_then(update::wanted_version),
        plugin,
        updating: false,
        width_requested: width::Requested::new(),
    };
    d.install_tmux_hooks().await;
    d.reconcile().await;
    let exit = loop {
        match rx.recv().await {
            Some(Cmd::Shutdown(why)) => break why,
            Some(cmd) => d.handle(cmd).await,
            None => break Exit::TmuxGone,
        }
    };
    match exit {
        Exit::TmuxGone => {
            log!("daemon exiting: tmux server gone");
            server::remove_socket(&d.opts.socket);
        }
        // The socket is the new daemon's to take; removing it could race its bind.
        Exit::Replaced => log!("daemon exiting: replaced by another build"),
    }
    Ok(())
}

async fn ticker(tx: mpsc::Sender<Cmd>) {
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        if tx.send(Cmd::Tick).await.is_err() {
            return;
        }
    }
}

/// The daemon may be started from a hook inside a pane; closing that pane must not kill it.
async fn ignore_sighup() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::hangup()) {
        Ok(mut hup) => while hup.recv().await.is_some() {},
        Err(e) => log!("failed to install SIGHUP handler: {e}"),
    }
}

impl Daemon {
    async fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Hook(env) => self.on_hook(env),
            Cmd::List(reply) => {
                // The requester may have hung up; there is no one to tell.
                let _ = reply.send(self.status());
            }
            Cmd::Subscribe(reply) => {
                let _ = reply.send((self.status(), self.updates.subscribe()));
            }
            Cmd::Refresh => {
                self.focus_events = self.query_focus_events().await;
                self.schedule_reconcile();
            }
            Cmd::TmuxNotification { name } => {
                if control_mode::triggers_reconcile(&name) {
                    self.schedule_reconcile();
                }
            }
            Cmd::Reconcile => {
                self.reconcile_pending = false;
                self.reconcile().await;
            }
            Cmd::Tick => {
                self.read_live().await;
                self.scrape().await;
                // tmux sends no notification when a pane changes directory or foreground
                // process, which pane rows show. With the sidebar hidden, only processes are
                // checked, and less often, so one that ends meanwhile is still marked when it
                // opens without the cost of a check every second.
                let every = Duration::from_secs(self.opts.config.panes.hidden_check_secs);
                if self.sidebar_on {
                    self.schedule_reconcile();
                } else if check_due(self.runs_checked, Instant::now(), every) {
                    self.poll_runs().await;
                }
            }
            Cmd::Turn { pane, turn } => {
                self.turn_inflight.remove(&pane);
                let turn = turn.unwrap_or_default();
                if self.store.set_model(&pane, turn.model, turn.effort) {
                    self.publish();
                }
            }
            Cmd::Git { cwd, status } => {
                self.git_inflight.remove(&cwd);
                let branch = status.as_ref().and_then(|s| s.branch.clone());
                let changed = self.git.get(&cwd).is_none_or(|(_, old)| *old != status);
                self.git.insert(cwd.clone(), (Instant::now(), status));
                if self.store.set_branch(&cwd, branch) || changed {
                    self.publish();
                }
            }
            Cmd::Toggle(reply) => {
                let on = self.toggle().await;
                let _ = reply.send(on);
            }
            Cmd::Resized => self.schedule_width_check(),
            Cmd::CheckWidths => {
                self.widths_pending = false;
                self.check_widths().await;
            }
            Cmd::Collapse {
                session,
                collapsed,
                reply,
            } => {
                let ok = match self.set_collapsed(&session, collapsed).await {
                    Ok(()) => true,
                    Err(e) => {
                        log!("failed to set {COLLAPSED} on session {session}: {e:#}");
                        false
                    }
                };
                let _ = reply.send(ok);
            }
            Cmd::Identify(reply) => {
                let _ = reply.send(self.opts.identity.clone());
            }
            Cmd::PluginChanged => self.on_plugin_changed(),
            Cmd::Installed { version, result } => self.on_installed(&version, result),
            Cmd::Shutdown(_) => {}
        }
    }

    /// Installs the release the plugin checkout now names, once per VERSION change.
    fn on_plugin_changed(&mut self) {
        let Some(plugin) = self.plugin.clone() else {
            return;
        };
        let wanted = update::wanted_version(&plugin);
        if self.updating || wanted.is_none() || wanted == self.plugin_version {
            return;
        }
        self.plugin_version.clone_from(&wanted);
        let Some(version) = wanted.filter(|v| v != env!("CARGO_PKG_VERSION")) else {
            return;
        };
        log!("plugin updated to {version}; installing it");
        self.updating = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = update::install(&plugin).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Cmd::Installed { version, result }).await;
        });
    }

    /// Re-executes into the freshly installed binary, which takes over from tmux's state.
    fn on_installed(&mut self, version: &str, result: Result<(), String>) {
        self.updating = false;
        if let Err(e) = result {
            log!("failed to install {version}: {e}");
            return;
        }
        log!(
            "installed {version}; restarting as {}",
            self.opts.exe.display()
        );
        let e = update::exec(&self.opts.exe);
        log!("{e:#}");
    }

    fn status(&self) -> Status {
        Status {
            now_ms: now_ms(),
            sidebar: self.sidebar_on,
            panes: self.store.records(),
            windows: self.published_windows(),
            collapsed: self.collapsed.iter().cloned().collect(),
        }
    }

    fn publish(&self) {
        // No subscribers is the normal case when no sidebar is open.
        let _ = self.updates.send(self.status());
    }

    fn on_hook(&mut self, env: Envelope) {
        let Some(agent) = Agent::from_name(&env.agent) else {
            log!("ignoring hook from unknown agent {:?}", env.agent);
            return;
        };
        let Some(pane) = env.pane else {
            return;
        };
        let parsed = agent.parse(&env.event, &env.payload);
        let viewed = self.snapshot.viewed.contains(&pane);
        let transition = self.store.apply(&pane, agent, &parsed, now_ms(), viewed);
        if let Some(cwd) = self.store.get(&pane).and_then(|r| r.cwd.clone()) {
            let refresh = matches!(parsed.event, AgentEvent::SessionStart | AgentEvent::Stop)
                || !self.git.contains_key(&cwd);
            if refresh {
                self.fetch_git(cwd);
            }
        }
        if let Some(rec) = self.store.get(&pane)
            && rec.agent == Agent::Claude
            && let Some(transcript) = rec.transcript.clone()
            && (parsed.event == AgentEvent::Stop || rec.model.is_none())
        {
            self.read_turn(pane.clone(), transcript);
        }
        let located = match self.snapshot.panes.iter().find(|p| p.id == pane) {
            Some(info) => {
                self.store.locate(info);
                true
            }
            None => {
                self.schedule_reconcile();
                false
            }
        };
        self.on_transitions(transition.into_iter().collect(), &env.event);
        // Unplaced, the pane would show under a placeholder header; the reconcile publishes it.
        if located {
            self.publish();
        }
    }

    fn on_transitions(&mut self, transitions: Vec<Transition>, cause: &str) {
        for t in transitions {
            log!(
                "{} {}: {} -> {} ({cause})",
                t.pane,
                t.agent.name(),
                t.from.map_or("new", |s| s.name()),
                t.to.name()
            );
            let viewed = self.snapshot.viewed.contains(&t.pane);
            let cfg = &self.opts.config.notify;
            if cfg.enabled
                && self
                    .notify_gate
                    .admit(&t, viewed, now_ms(), cfg.dedupe_secs * 1000)
            {
                let msg = notify::message(&t, self.store.get(&t.pane));
                tokio::spawn(notify::deliver(self.opts.tmux.clone(), msg, cfg.clone()));
            }
        }
    }

    /// The published windows, each pane with the git status of its directory and what its
    /// process is doing.
    fn published_windows(&self) -> Vec<WindowInfo> {
        let mut windows = self.windows.clone();
        for p in windows.iter_mut().flat_map(|w| &mut w.panes) {
            p.git = self.git.get(&p.cwd).and_then(|(_, s)| s.clone());
            p.run = self.runs.get(&p.id).copied();
        }
        windows
    }

    /// Looks up agents' directories once, since panes found by the startup scan send no hook
    /// until used, and while the sidebar shows them, every pane's directory as it goes stale.
    fn refresh_git(&mut self) {
        let agents: HashSet<String> = self
            .store
            .records()
            .into_iter()
            .filter_map(|r| r.cwd)
            .collect();
        let panes: HashSet<String> = if self.sidebar_on {
            self.windows
                .iter()
                .flat_map(|w| &w.panes)
                .map(|p| p.cwd.clone())
                .collect()
        } else {
            HashSet::new()
        };
        self.git
            .retain(|cwd, _| agents.contains(cwd) || panes.contains(cwd));
        let stale: Vec<String> = agents
            .iter()
            .filter(|cwd| !self.git.contains_key(*cwd))
            .chain(panes.iter().filter(|cwd| {
                self.git
                    .get(*cwd)
                    .is_none_or(|(at, _)| at.elapsed() >= GIT_REFRESH)
            }))
            .cloned()
            .collect();
        for cwd in stale {
            self.fetch_git(cwd);
        }
    }

    /// Reads the model and effort of the pane's last reply, which tracks `/model` switches.
    fn read_turn(&mut self, pane: String, transcript: String) {
        if !self.turn_inflight.insert(pane.clone()) {
            return;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let path = PathBuf::from(&transcript);
            let turn = match tokio::task::spawn_blocking(move || last_turn(&path)).await {
                Ok(Ok(turn)) => turn,
                Ok(Err(e)) => {
                    log!("{e:#}");
                    None
                }
                Err(e) => {
                    log!("transcript reader for {pane} panicked: {e}");
                    None
                }
            };
            // A closed channel means shutdown.
            let _ = tx.send(Cmd::Turn { pane, turn }).await;
        });
    }

    fn fetch_git(&mut self, cwd: String) {
        if cwd.is_empty() || !self.git_inflight.insert(cwd.clone()) {
            return;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let status = match crate::git::status(&cwd).await {
                Ok(s) => s,
                Err(e) => {
                    log!("{e:#}");
                    None
                }
            };
            // The daemon loop only stops at shutdown, when the status no longer matters.
            let _ = tx.send(Cmd::Git { cwd, status }).await;
        });
    }

    /// Layout hooks fire on every step of a border drag, so checks are debounced, and skipped
    /// while no sidebar is shown.
    fn schedule_width_check(&mut self) {
        if !self.sidebar_on || self.widths_pending {
            return;
        }
        self.widths_pending = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RECONCILE_DEBOUNCE).await;
            // See fetch_git: a closed channel means shutdown.
            let _ = tx.send(Cmd::CheckWidths).await;
        });
    }

    /// Reasserts widths from a fresh look at the panes, without a full reconcile.
    async fn check_widths(&mut self) {
        match self.opts.tmux.snapshot(self.focus_events).await {
            Ok(snap) => self.reassert_widths(&snap.panes).await,
            Err(e) => log!("failed to read panes to check sidebar widths: {e:#}"),
        }
    }

    fn schedule_reconcile(&mut self) {
        if self.reconcile_pending {
            return;
        }
        self.reconcile_pending = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RECONCILE_DEBOUNCE).await;
            // See fetch_git: a closed channel means shutdown.
            let _ = tx.send(Cmd::Reconcile).await;
        });
    }

    async fn reconcile(&mut self) {
        let snap = match self.opts.tmux.snapshot(self.focus_events).await {
            Ok(s) => s,
            Err(e) => {
                log!("reconcile failed: {e:#}");
                return;
            }
        };
        let now = now_ms();
        if !self.recovered {
            self.recovered = true;
            // Sidebars outlive a daemon restart; adopt them so the next toggle hides them.
            self.sidebar_on = snap.panes.iter().any(|p| p.is_sidebar);
            let added = self.store.recover(&snap, now);
            if !added.is_empty() {
                log!("recovered agent panes: {}", added.join(" "));
            }
            self.restart_stale_sidebars().await;
            self.set_layout_hook(self.sidebar_on).await;
        }
        let transitions = self.store.reconcile(&snap, now);
        self.windows = tmux::windows(&snap);
        self.collapsed = snap
            .panes
            .iter()
            .filter(|p| p.session_collapsed)
            .map(|p| p.session.clone())
            .collect();
        self.snapshot = snap;
        self.apply_runs(now);
        self.refresh_git();
        self.maintain_sidebars().await;
        // Layout changes in the control client's session trigger a reconcile, so this also
        // snaps those sidebars back to their share; the layout hook covers other sessions.
        let panes = self.snapshot.panes.clone();
        self.reassert_widths(&panes).await;
        self.on_transitions(transitions, "viewed");
        self.publish();
    }

    /// Takes a snapshot for `apply_runs` alone, without the rest of a reconcile.
    async fn poll_runs(&mut self) {
        match self.opts.tmux.snapshot(self.focus_events).await {
            Ok(snap) => {
                self.snapshot = snap;
                self.apply_runs(now_ms());
            }
            Err(e) => log!("failed to read panes to poll their processes: {e:#}"),
        }
    }

    /// Reads what each plain pane's foreground process is doing in the current snapshot,
    fn apply_runs(&mut self, now: u64) {
        self.runs_checked = Some(Instant::now());
        let store = &self.store;
        self.runs.update(
            &self.snapshot,
            |p| store.get(&p.id).is_some() || Agent::for_process(&p.current_command).is_some(),
            &self.opts.config.panes.interactive,
            now,
        );
    }

    /// Applies Claude's and Kiro's own session status; it covers Esc, denials, interrupts, and
    /// Kiro's approvals, which fire no hook.
    async fn read_live(&mut self) {
        let claude_dir = self.sessions_dir.clone();
        let kiro_dir = self.kiro_sessions_dir.clone();
        if claude_dir.is_none() && kiro_dir.is_none() {
            return;
        }
        let kiro_panes = self.store.kiro_panes();
        let read = tokio::task::spawn_blocking(move || {
            let mut found = claude_sessions::Sessions::default();
            let mut merge = |read: Result<claude_sessions::Sessions>| match read {
                Ok(s) => {
                    found.live.extend(s.live);
                    found.skipped.extend(s.skipped);
                }
                Err(e) => found.skipped.push(format!("{e:#}")),
            };
            if let Some(dir) = claude_dir {
                merge(claude_sessions::read(&dir, claude_sessions::pid_alive));
            }
            if let Some(dir) = kiro_dir {
                merge(kiro_sessions::read(
                    &dir,
                    &kiro_panes,
                    claude_sessions::pid_alive,
                ));
            }
            found
        })
        .await;
        // Panes a failed source covered lose their live status, so the screen scrape takes over.
        let (live, error) = match read {
            Ok(found) => {
                let error = (!found.skipped.is_empty()).then(|| found.skipped.join("; "));
                (found.live, error)
            }
            Err(e) => (Vec::new(), Some(format!("session reader panicked: {e}"))),
        };
        if error.is_some() && error != self.live_error {
            log!("{}", error.as_deref().unwrap_or_default());
        }
        self.live_error = error;
        let changed = self.store.apply_live(&live, now_ms());
        if !changed.is_empty() {
            self.on_transitions(changed, "session");
            self.publish();
        }
    }

    async fn scrape(&mut self) {
        let due = self
            .store
            .needs_scrape(now_ms(), self.opts.config.blocked_threshold_ms);
        let mut changed = Vec::new();
        for (pane, agent) in due {
            match scan::screen(&self.opts.tmux, &pane, agent, &self.opts.config).await {
                Ok(screen) => changed.extend(self.store.apply_screen(&pane, screen, now_ms())),
                Err(e) => log!("scrape of {pane} failed: {e:#}"),
            }
        }
        if !changed.is_empty() {
            self.on_transitions(changed, "screen");
            self.publish();
        }
    }

    async fn query_focus_events(&self) -> bool {
        match self
            .opts
            .tmux
            .run_async(&["show-options", "-gv", "focus-events"])
            .await
        {
            Ok(v) => v.trim() == "on",
            Err(e) => {
                log!("{e:#}");
                false
            }
        }
    }

    /// tmux sends no control-mode notification for terminal focus, nor `%layout-change` for
    /// windows outside the control client's session, so hooks poke the daemon.
    async fn install_tmux_hooks(&mut self) {
        self.focus_events = self.query_focus_events().await;
        let refresh = self.hook_command("refresh");
        let resized = self.hook_command("resized");
        for (hook, run) in [
            ("pane-focus-in", &refresh),
            ("pane-focus-out", &refresh),
            ("client-resized", &resized),
        ] {
            let name = format!("{hook}[{HOOK_INDEX}]");
            if let Err(e) = self
                .opts
                .tmux
                .run_async(&["set-hook", "-g", &name, run])
                .await
            {
                log!("failed to install tmux hook {name}: {e:#}");
            }
        }
    }

    /// A hook body that runs `simul internal <sub>` against this daemon.
    fn hook_command(&self, sub: &str) -> String {
        let cmd = format!(
            "SIMUL_SOCKET={} {} internal {sub} >/dev/null 2>&1",
            shell_quote(&self.opts.socket.to_string_lossy()),
            shell_quote(&self.opts.exe.to_string_lossy()),
        );
        format!("run-shell -b {}", tmux_double_quote(&cmd))
    }

    /// Installs, or removes, the hook that reports a dragged sidebar border in any session.
    /// It fires on every layout change, so it is only set while sidebars are shown.
    async fn set_layout_hook(&self, on: bool) {
        let name = format!("{LAYOUT_HOOK}[{HOOK_INDEX}]");
        let run = self.hook_command("resized");
        let args: &[&str] = if on {
            &["set-hook", "-g", &name, &run]
        } else {
            &["set-hook", "-gu", &name]
        };
        if let Err(e) = self.opts.tmux.run_async(args).await {
            log!(
                "failed to set tmux hook {name} {}: {e:#}",
                if on { "on" } else { "off" }
            );
        }
    }

    fn windows(&self) -> HashSet<String> {
        self.snapshot
            .panes
            .iter()
            .map(|p| p.window_id.clone())
            .collect()
    }

    fn windows_with_sidebar(&self) -> HashSet<String> {
        self.snapshot
            .panes
            .iter()
            .filter(|p| p.is_sidebar)
            .map(|p| p.window_id.clone())
            .collect()
    }

    async fn toggle(&mut self) -> bool {
        self.reconcile().await;
        if self.sidebar_on {
            self.sidebar_on = false;
            for pane in self.snapshot.panes.iter().filter(|p| p.is_sidebar) {
                if let Err(e) = self
                    .opts
                    .tmux
                    .run_async(&["kill-pane", "-t", &pane.id])
                    .await
                {
                    log!("failed to close sidebar {}: {e:#}", pane.id);
                }
            }
            self.set_layout_hook(false).await;
            // Regardless of config, so turning hide_status off still brings the line back.
            if let Err(e) = self.restore_status().await {
                log!("failed to restore the status line: {e:#}");
            }
        } else {
            self.sidebar_on = true;
            let have = self.windows_with_sidebar();
            for window in self.windows().difference(&have) {
                self.add_sidebar(window).await;
            }
            self.set_layout_hook(true).await;
            if self.opts.config.sidebar.hide_status
                && let Err(e) = self.hide_status().await
            {
                log!("failed to hide the status line: {e:#}");
            }
        }
        self.known_windows = self.windows();
        self.publish();
        self.schedule_reconcile();
        self.sidebar_on
    }

    /// Folds or unfolds `session` in every sidebar. tmux sends no notification for an option
    /// change, so this publishes the change itself.
    async fn set_collapsed(&mut self, session: &str, collapsed: bool) -> Result<()> {
        // set-option takes a pane target; the trailing `:` makes it name a session (its current
        // window and pane), and `=` makes tmux match the name exactly, even one like "3".
        let target = format!("={session}:");
        let args: &[&str] = if collapsed {
            &["set-option", "-t", &target, COLLAPSED, "1"]
        } else {
            &["set-option", "-uq", "-t", &target, COLLAPSED]
        };
        self.opts.tmux.run_async(args).await?;
        if collapsed {
            self.collapsed.insert(session.to_owned());
        } else {
            self.collapsed.remove(session);
        }
        self.publish();
        Ok(())
    }

    /// The status-format[0] that hide_status replaced. It lives in tmux so a daemon that
    /// adopts sidebars after a restart can still restore it.
    async fn saved_status(&self) -> Result<Option<String>> {
        let tmux = &self.opts.tmux;
        // Without -v, an unset option prints nothing; with -v, it can't be told from "".
        if tmux
            .run_async(&["show-options", "-gq", SAVED_STATUS])
            .await?
            .is_empty()
        {
            return Ok(None);
        }
        let value = tmux
            .run_async(&["show-options", "-gqv", SAVED_STATUS])
            .await?;
        Ok(Some(strip_newline(value)))
    }

    /// Cuts tmux's first status line down to status-right; later lines keep running.
    async fn hide_status(&self) -> Result<()> {
        let tmux = &self.opts.tmux;
        // Already saved by an earlier daemon, whose cut-down line must not overwrite it.
        if self.saved_status().await?.is_none() {
            let current = tmux
                .run_async(&["show-options", "-gv", "status-format[0]"])
                .await?;
            tmux.run_async(&["set-option", "-g", SAVED_STATUS, &strip_newline(current)])
                .await?;
        }
        tmux.run_async(&["set-option", "-g", "status-format[0]", RIGHT_ONLY_STATUS])
            .await?;
        Ok(())
    }

    async fn restore_status(&self) -> Result<()> {
        let Some(saved) = self.saved_status().await? else {
            return Ok(());
        };
        let tmux = &self.opts.tmux;
        tmux.run_async(&["set-option", "-g", "status-format[0]", &saved])
            .await?;
        tmux.run_async(&["set-option", "-gu", SAVED_STATUS]).await?;
        Ok(())
    }

    /// Adds sidebars to new windows and closes sidebars left alone in their window.
    async fn maintain_sidebars(&mut self) {
        let windows = self.windows();
        if self.sidebar_on {
            let have = self.windows_with_sidebar();
            let new: Vec<String> = windows
                .difference(&self.known_windows)
                .filter(|w| !have.contains(*w))
                .cloned()
                .collect();
            for window in &new {
                self.add_sidebar(window).await;
            }
        }
        self.known_windows = windows;
        let lone: Vec<String> = self
            .snapshot
            .panes
            .iter()
            .filter(|p| p.is_sidebar && p.window_panes == 1)
            .map(|p| p.id.clone())
            .collect();
        for pane in lone {
            if let Err(e) = self.opts.tmux.run_async(&["kill-pane", "-t", &pane]).await {
                log!("failed to close lone sidebar {pane}: {e:#}");
            }
        }
    }

    /// The shell command a sidebar pane runs.
    fn sidebar_command(&self) -> String {
        format!(
            "SIMUL_SOCKET={} exec {} sidebar",
            shell_quote(&self.opts.socket.to_string_lossy()),
            shell_quote(&self.opts.exe.to_string_lossy()),
        )
    }

    async fn tag_sidebar_build(&self, pane: &str) {
        let tag = self.opts.identity.tag();
        let set = ["set-option", "-p", "-t", pane, SIDEBAR_EXE, &tag];
        if let Err(e) = self.opts.tmux.run_async(&set).await {
            log!("failed to tag sidebar {pane} with its build: {e:#}");
        }
    }

    /// Restarts in place each sidebar left by another build, such as the daemon this one
    /// replaced, so the sidebar runs this build's code too.
    async fn restart_stale_sidebars(&self) {
        let format = format!("#{{pane_id}}\t#{{@simul-sidebar}}\t#{{{SIDEBAR_EXE}}}");
        let out = match self
            .opts
            .tmux
            .run_async(&["list-panes", "-a", "-F", &format])
            .await
        {
            Ok(out) => out,
            Err(e) => {
                log!("failed to list sidebars: {e:#}");
                return;
            }
        };
        let tag = self.opts.identity.tag();
        let mut seen = HashSet::new();
        let cmd = self.sidebar_command();
        for line in out.lines() {
            let mut f = line.splitn(3, '\t');
            let (Some(pane), Some("1"), build) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            // A linked window lists its panes once per session.
            if build == Some(tag.as_str()) || !seen.insert(pane) {
                continue;
            }
            let respawn = ["respawn-pane", "-k", "-t", pane, &cmd];
            match self.opts.tmux.run_async(&respawn).await {
                Ok(_) => self.tag_sidebar_build(pane).await,
                Err(e) => log!("failed to restart sidebar {pane}: {e:#}"),
            }
        }
    }

    async fn add_sidebar(&self, window: &str) {
        let cmd = self.sidebar_command();
        let split = [
            "split-window",
            "-hbfd",
            "-l",
            &width::split_size(),
            "-t",
            window,
            "-P",
            "-F",
            "#{pane_id}",
            &cmd,
        ];
        let pane = match self.opts.tmux.run_async(&split).await {
            Ok(p) => p.trim().to_owned(),
            Err(e) => {
                log!("failed to open sidebar in {window}: {e:#}");
                return;
            }
        };
        // The actor awaits this before handling the next reconcile, so no snapshot sees an
        // untagged sidebar and adds a second one.
        let tag = ["set-option", "-p", "-t", &pane, "@simul-sidebar", "1"];
        if let Err(e) = self.opts.tmux.run_async(&tag).await {
            log!("failed to tag sidebar {pane}: {e:#}");
            return;
        }
        self.tag_sidebar_build(&pane).await;
        // The split fired window-layout-changed before the tag existed; rerun the user's
        // hooks so any that skip sidebars (e.g. pane-count checks) see it. This also reruns
        // simul's own width hook, which is cheap: the width is already right.
        let rerun = ["set-hook", "-R", "-t", &pane, LAYOUT_HOOK];
        if let Err(e) = self.opts.tmux.run_async(&rerun).await {
            log!("failed to rerun layout hooks for {pane}: {e:#}");
        }
    }

    /// Brings every sidebar in `panes` back to its share of the window (see `width::resizes`).
    async fn reassert_widths(&mut self, panes: &[PaneInfo]) {
        for (pane, cols) in width::resizes(panes, &mut self.width_requested) {
            let cols = cols.to_string();
            let resize = ["resize-pane", "-t", &pane, "-x", &cols];
            if let Err(e) = self.opts.tmux.run_async(&resize).await {
                log!("failed to resize sidebar {pane} to {cols} columns: {e:#}");
            }
        }
    }
}

/// Whether a hidden-sidebar process check is due, `every` after the last look at `last`.
fn check_due(last: Option<Instant>, now: Instant, every: Duration) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= every)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_checks_wait_out_their_interval() {
        let t0 = Instant::now();
        let every = Duration::from_secs(10);
        assert!(check_due(None, t0, every), "first check runs at once");
        assert!(!check_due(Some(t0), t0 + Duration::from_secs(9), every));
        assert!(check_due(Some(t0), t0 + Duration::from_secs(10), every));
        assert!(
            check_due(Some(t0), t0, Duration::ZERO),
            "0 checks on every tick"
        );
    }
}
