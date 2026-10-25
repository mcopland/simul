pub mod control_mode;
pub mod notify;
pub mod scan;
pub mod server;
pub mod state;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::claude_metrics::{Turn, last_turn};
use crate::claude_sessions;
use crate::config::Config;
use crate::hook::{Agent, AgentEvent, Envelope};
use crate::log;
use crate::protocol::{Status, now_ms};
use crate::tmux::{Tmux, shell_quote, tmux_double_quote};
use state::{Snapshot, Store, Transition};

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(50);
const TICK: Duration = Duration::from_secs(1);
/// Index in tmux hook arrays, so simul's entries never replace the user's.
const HOOK_INDEX: u32 = 7463;

pub enum Cmd {
    Hook(Envelope),
    List(oneshot::Sender<Status>),
    Subscribe(oneshot::Sender<(Status, broadcast::Receiver<Status>)>),
    Refresh,
    Toggle(oneshot::Sender<bool>),
    Resized,
    TmuxNotification { name: String },
    Reconcile,
    Tick,
    Branch { cwd: String, branch: Option<String> },
    Turn { pane: String, turn: Option<Turn> },
    Shutdown,
}

pub struct Options {
    pub socket: PathBuf,
    pub tmux: Tmux,
    pub exe: PathBuf,
    pub config: Config,
}

struct Daemon {
    opts: Options,
    store: Store,
    snapshot: Snapshot,
    recovered: bool,
    focus_events: bool,
    tx: mpsc::Sender<Cmd>,
    updates: broadcast::Sender<Status>,
    reconcile_pending: bool,
    branch_inflight: HashSet<String>,
    sidebar_on: bool,
    /// Windows seen at the last reconcile; only windows new since then get a sidebar.
    known_windows: HashSet<String>,
    notify_gate: notify::Gate,
    /// Working directories whose branch has been looked up at least once.
    branch_fetched: HashSet<String>,
    /// Panes whose transcript is being read.
    turn_inflight: HashSet<String>,
    /// Where Claude Code keeps live session status; `None` without `$HOME`.
    sessions_dir: Option<PathBuf>,
    /// Last failure reading `sessions_dir`, so a persistent one is logged once.
    live_error: Option<String>,
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

    let (updates, _) = broadcast::channel(64);
    let mut d = Daemon {
        opts,
        store: Store::default(),
        snapshot: Snapshot::default(),
        recovered: false,
        focus_events: false,
        tx,
        updates,
        reconcile_pending: false,
        branch_inflight: HashSet::new(),
        sidebar_on: false,
        known_windows: HashSet::new(),
        notify_gate: notify::Gate::default(),
        branch_fetched: HashSet::new(),
        turn_inflight: HashSet::new(),
        sessions_dir: claude_sessions::sessions_dir(crate::paths::env_var),
        live_error: None,
    };
    d.install_tmux_hooks().await;
    d.reconcile().await;
    while let Some(cmd) = rx.recv().await {
        if matches!(cmd, Cmd::Shutdown) {
            break;
        }
        d.handle(cmd).await;
    }
    log!("daemon exiting: tmux server gone");
    server::remove_socket(&d.opts.socket);
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
            }
            Cmd::Turn { pane, turn } => {
                self.turn_inflight.remove(&pane);
                let turn = turn.unwrap_or_default();
                if self.store.set_model(&pane, turn.model, turn.effort) {
                    self.publish();
                }
            }
            Cmd::Branch { cwd, branch } => {
                self.branch_inflight.remove(&cwd);
                if self.store.set_branch(&cwd, branch) {
                    self.publish();
                }
            }
            Cmd::Toggle(reply) => {
                let on = self.toggle().await;
                let _ = reply.send(on);
            }
            Cmd::Resized => self.reassert_widths().await,
            Cmd::Shutdown => {}
        }
    }

    fn status(&self) -> Status {
        Status {
            now_ms: now_ms(),
            sidebar: self.sidebar_on,
            panes: self.store.records(),
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
                || !self.branch_fetched.contains(&cwd);
            if refresh {
                self.fetch_branch(cwd);
            }
        }
        if let Some(rec) = self.store.get(&pane)
            && rec.agent == Agent::Claude
            && let Some(transcript) = rec.transcript.clone()
            && (parsed.event == AgentEvent::Stop || rec.model.is_none())
        {
            self.read_turn(pane.clone(), transcript);
        }
        match self.snapshot.panes.iter().find(|p| p.id == pane) {
            Some(info) => self.store.locate(info),
            None => self.schedule_reconcile(),
        }
        self.on_transitions(transition.into_iter().collect(), &env.event);
        self.publish();
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

    /// Panes found by the startup scan send no hook until used, so look their branch up here.
    fn fetch_missing_branches(&mut self) {
        let missing: HashSet<String> = self
            .store
            .records()
            .into_iter()
            .filter_map(|r| r.cwd)
            .filter(|cwd| !self.branch_fetched.contains(cwd))
            .collect();
        for cwd in missing {
            self.fetch_branch(cwd);
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

    fn fetch_branch(&mut self, cwd: String) {
        if !self.branch_inflight.insert(cwd.clone()) {
            return;
        }
        self.branch_fetched.insert(cwd.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let branch = match crate::git::current_branch(&cwd).await {
                Ok(b) => b,
                Err(e) => {
                    log!("{e:#}");
                    None
                }
            };
            // The daemon loop only stops at shutdown, when the branch no longer matters.
            let _ = tx.send(Cmd::Branch { cwd, branch }).await;
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
            // See fetch_branch: a closed channel means shutdown.
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
        }
        let transitions = self.store.reconcile(&snap, now);
        self.snapshot = snap;
        self.fetch_missing_branches();
        self.maintain_sidebars().await;
        self.on_transitions(transitions, "viewed");
        self.publish();
    }

    /// Applies Claude's own session status; it covers Esc, denials, and interrupts, which fire no hook.
    async fn read_live(&mut self) {
        let Some(dir) = self.sessions_dir.clone() else {
            return;
        };
        let read = tokio::task::spawn_blocking(move || {
            claude_sessions::read(&dir, claude_sessions::pid_alive)
        })
        .await;
        // On failure every pane loses its live status, so the screen scrape takes over.
        let (live, error) = match read {
            Ok(Ok(found)) => {
                let error = (!found.skipped.is_empty()).then(|| found.skipped.join("; "));
                (found.live, error)
            }
            Ok(Err(e)) => (Vec::new(), Some(format!("{e:#}"))),
            Err(e) => (
                Vec::new(),
                Some(format!("Claude session reader panicked: {e}")),
            ),
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
            match scan::screen(&self.opts.tmux, &pane, agent).await {
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

    /// tmux sends no control-mode notification for terminal focus, so hooks poke the daemon.
    async fn install_tmux_hooks(&mut self) {
        self.focus_events = self.query_focus_events().await;
        let cmd = format!(
            "SIMUL_SOCKET={} {} internal refresh >/dev/null 2>&1",
            shell_quote(&self.opts.socket.to_string_lossy()),
            shell_quote(&self.opts.exe.to_string_lossy()),
        );
        let refresh = format!("run-shell -b {}", tmux_double_quote(&cmd));
        let resized = refresh.replace("internal refresh", "internal resized");
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
        } else {
            self.sidebar_on = true;
            let have = self.windows_with_sidebar();
            for window in self.windows().difference(&have) {
                self.add_sidebar(window).await;
            }
        }
        self.known_windows = self.windows();
        self.publish();
        self.schedule_reconcile();
        self.sidebar_on
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

    async fn add_sidebar(&self, window: &str) {
        let width = self.opts.config.sidebar.width.to_string();
        let cmd = format!(
            "SIMUL_SOCKET={} exec {} sidebar",
            shell_quote(&self.opts.socket.to_string_lossy()),
            shell_quote(&self.opts.exe.to_string_lossy()),
        );
        let split = [
            "split-window",
            "-hbfd",
            "-l",
            &width,
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
        // The split fired window-layout-changed before the tag existed; rerun the user's
        // hooks so any that skip sidebars (e.g. pane-count checks) see it.
        let rerun = ["set-hook", "-R", "-t", &pane, "window-layout-changed"];
        if let Err(e) = self.opts.tmux.run_async(&rerun).await {
            log!("failed to rerun layout hooks for {pane}: {e:#}");
        }
    }

    async fn reassert_widths(&mut self) {
        let width = self.opts.config.sidebar.width.to_string();
        for pane in self.snapshot.panes.iter().filter(|p| p.is_sidebar) {
            let resize = ["resize-pane", "-t", &pane.id, "-x", &width];
            if let Err(e) = self.opts.tmux.run_async(&resize).await {
                log!("failed to resize sidebar {}: {e:#}", pane.id);
            }
        }
    }
}
