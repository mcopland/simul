pub mod control_mode;
pub mod scan;
pub mod server;
pub mod state;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, oneshot};

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
    TmuxNotification { name: String },
    Reconcile,
    Tick,
    Branch { cwd: String, branch: Option<String> },
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
            Cmd::Tick => self.scrape().await,
            Cmd::Branch { cwd, branch } => {
                self.branch_inflight.remove(&cwd);
                if self.store.set_branch(&cwd, branch) {
                    self.publish();
                }
            }
            Cmd::Shutdown => {}
        }
    }

    fn status(&self) -> Status {
        Status {
            now_ms: now_ms(),
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
        if let Some(rec) = self.store.get(&pane) {
            let refresh_branch =
                matches!(parsed.event, AgentEvent::SessionStart | AgentEvent::Stop)
                    || rec.branch.is_none();
            if let (true, Some(cwd)) = (refresh_branch, rec.cwd.clone()) {
                self.fetch_branch(cwd);
            }
        }
        match self.snapshot.panes.iter().find(|p| p.id == pane) {
            Some(info) => self.store.locate(info),
            None => self.schedule_reconcile(),
        }
        self.on_transitions(transition.into_iter().collect());
        self.publish();
    }

    fn on_transitions(&mut self, transitions: Vec<Transition>) {
        for t in transitions {
            log!(
                "{} {}: {} -> {}",
                t.pane,
                t.agent.name(),
                t.from.map_or("new", |s| s.name()),
                t.to.name()
            );
        }
    }

    fn fetch_branch(&mut self, cwd: String) {
        if !self.branch_inflight.insert(cwd.clone()) {
            return;
        }
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
            let added = self.store.recover(&snap, now);
            if !added.is_empty() {
                log!("recovered agent panes: {}", added.join(" "));
            }
        }
        let transitions = self.store.reconcile(&snap, now);
        self.snapshot = snap;
        self.on_transitions(transitions);
        self.publish();
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
            self.on_transitions(changed);
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
        let run = format!("run-shell -b {}", tmux_double_quote(&cmd));
        for hook in ["pane-focus-in", "pane-focus-out"] {
            let name = format!("{hook}[{HOOK_INDEX}]");
            if let Err(e) = self
                .opts
                .tmux
                .run_async(&["set-hook", "-g", &name, &run])
                .await
            {
                log!("failed to install tmux hook {name}: {e:#}");
            }
        }
    }
}
