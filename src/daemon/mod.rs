pub mod control_mode;
mod git_cache;
mod live;
pub mod notify;
pub mod runs;
pub mod scan;
pub mod server;
mod sidebar;
pub mod state;
mod tmux_hooks;
pub mod update;
pub mod width;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::claude_metrics::{Turn, last_turn};
use crate::claude_sessions;
use crate::config::Config;
use crate::hook::AgentEvent;
use crate::kiro_sessions;
use crate::log;
use crate::protocol::{Agent, Envelope, GitStatus, Identity, Status, WindowInfo, now_ms};
use crate::tmux::{self, Snapshot, Tmux};
use git_cache::GitCache;
use live::LiveFiles;
use runs::Runs;
use sidebar::Sidebars;
use state::{Store, Transition};
use update::Updater;

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(50);
const TICK: Duration = Duration::from_secs(1);
/// Index in tmux hook arrays, so simul's entries never replace the user's.
const HOOK_INDEX: u32 = 7463;
const LAYOUT_HOOK: &str = "window-layout-changed";
/// Session option marking a session folded in the sidebar. It lives in tmux so it survives
/// daemon restarts, goes with its session, and follows renames.
const COLLAPSED: &str = "@simul-collapsed";

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
    /// A retry armed after a failed install or restart (see `Updater::retry_due`).
    RetryInstall(u64),
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
    /// The version this daemon was installed as before an update restarted it.
    pub restarted_for: Option<String>,
}

struct Daemon {
    opts: Options,
    store: Store,
    /// Foreground processes of panes without an agent.
    runs: Runs,
    snapshot: Snapshot,
    /// The snapshot's windows as published, rebuilt at each reconcile.
    windows: Vec<WindowInfo>,
    recovered: bool,
    focus_events: bool,
    tx: mpsc::Sender<Cmd>,
    updates: broadcast::Sender<Status>,
    reconcile_pending: bool,
    sidebar: Sidebars,
    notify_gate: notify::Gate,
    git: GitCache,
    /// Panes whose transcript is being read.
    turn_inflight: HashSet<String>,
    live: LiveFiles,
    updater: Updater,
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
        snapshot: Snapshot::default(),
        windows: Vec::new(),
        recovered: false,
        focus_events: false,
        tx,
        updates,
        reconcile_pending: false,
        sidebar: Sidebars::default(),
        notify_gate: notify::Gate::default(),
        git: GitCache::default(),
        turn_inflight: HashSet::new(),
        live: LiveFiles::new(
            claude_sessions::sessions_dir(crate::paths::env_var),
            kiro_sessions::sessions_dir(crate::paths::env_var),
        ),
        updater: Updater::new(plugin, env!("CARGO_PKG_VERSION")),
    };
    d.install_tmux_hooks().await;
    d.reconcile().await;
    if let Some(version) = d.opts.restarted_for.clone()
        && d.updater.restarted_for(&version)
    {
        log!(
            "installed {version}, but the new build reports {}; leaving it until VERSION changes",
            env!("CARGO_PKG_VERSION")
        );
    }
    // The checkout may have moved on while no daemon was watching it.
    d.on_plugin_changed();
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
                if self.sidebar.on {
                    self.schedule_reconcile();
                } else if self.runs.due(Instant::now(), every) {
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
                let branch = status.as_ref().and_then(|s| s.branch.clone());
                let changed = self.git.finish(cwd.clone(), status, Instant::now());
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
                self.sidebar.widths_pending = false;
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
            Cmd::RetryInstall(retry) => self.on_retry_install(retry),
            Cmd::Installed { version, result } => self.on_installed(&version, result),
            Cmd::Shutdown(_) => {}
        }
    }

    fn status(&self) -> Status {
        Status {
            now_ms: now_ms(),
            sidebar: self.sidebar.on,
            panes: self.store.records(),
            windows: self.published_windows(),
            collapsed: self.sidebar.collapsed.iter().cloned().collect(),
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
                || !self.git.knows(&cwd);
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
            p.git = self.git.get(&p.cwd).cloned();
            p.run = self.runs.get(&p.id).copied();
        }
        windows
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

    fn schedule_reconcile(&mut self) {
        if self.reconcile_pending {
            return;
        }
        self.reconcile_pending = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RECONCILE_DEBOUNCE).await;
            // The daemon loop only stops at shutdown, when this no longer matters.
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
            self.sidebar.on = snap.panes.iter().any(|p| p.is_sidebar);
            let added = self.store.recover(&snap, now);
            if !added.is_empty() {
                log!("recovered agent panes: {}", added.join(" "));
            }
            self.restart_stale_sidebars().await;
            self.set_layout_hook(self.sidebar.on).await;
        }
        let transitions = self.store.reconcile(&snap, now);
        self.windows = tmux::windows(&snap);
        self.sidebar.collapsed = snap
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
        self.runs.mark_checked(Instant::now());
        let store = &self.store;
        self.runs.update(
            &self.snapshot,
            |p| store.get(&p.id).is_some() || Agent::for_process(&p.current_command).is_some(),
            &self.opts.config.panes.interactive,
            now,
        );
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
}
