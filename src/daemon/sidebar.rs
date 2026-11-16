//! The sidebar panes: opening and closing them in every window, keeping their width, folding
//! sessions, and hiding the status line they repeat.

use std::collections::{BTreeSet, HashSet};

use anyhow::Result;

use super::{COLLAPSED, Cmd, Daemon, LAYOUT_HOOK, RECONCILE_DEBOUNCE, width};
use crate::log;
use crate::tmux::{PaneInfo, SIDEBAR_TITLE, Snapshot, chain, shell_quote};

/// Global tmux option holding the status-format[0] hidden while the sidebar is on.
const SAVED_STATUS: &str = "@simul-status-format";

/// Pane option naming the build a sidebar runs (see `Identity::tag`), so a daemon that takes
/// over can restart sidebars still running an older one.
const SIDEBAR_EXE: &str = "@simul-sidebar-exe";

/// The tmux commands that mark `pane` as a sidebar running `build` (see `Identity::tag`): the
/// tag the daemon finds sidebars by, the build, and the title a session restore keeps when it
/// drops pane options (see `PaneInfo::restored_sidebar`).
fn sidebar_marks<'a>(pane: &'a str, build: &'a str) -> [Vec<&'a str>; 3] {
    [
        vec!["set-option", "-p", "-t", pane, "@simul-sidebar", "1"],
        vec!["set-option", "-p", "-t", pane, SIDEBAR_EXE, build],
        vec!["select-pane", "-t", pane, "-T", SIDEBAR_TITLE],
    ]
}

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

#[derive(Debug, Default)]
pub struct Sidebars {
    pub on: bool,
    /// Windows seen at the last reconcile; only windows new since then get a sidebar.
    pub known_windows: HashSet<String>,
    /// Sessions folded in the sidebar, as read from `COLLAPSED` at the last reconcile.
    pub collapsed: BTreeSet<String>,
    pub widths_pending: bool,
    /// Sidebar widths last asked of tmux (see `width::resizes`).
    pub width_requested: width::Requested,
}

impl Daemon {
    /// Layout hooks fire on every step of a border drag, so checks are debounced, and skipped
    /// while no sidebar is shown. A pending reconcile reasserts widths itself, and a layout
    /// change in the control client's session has scheduled one before its hook arrives.
    pub(super) fn schedule_width_check(&mut self) {
        if !self.sidebar.on || self.sidebar.widths_pending || self.reconcile_pending {
            return;
        }
        self.sidebar.widths_pending = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RECONCILE_DEBOUNCE).await;
            // The daemon loop only stops at shutdown, when this no longer matters.
            let _ = tx.send(Cmd::CheckWidths).await;
        });
    }

    /// Reasserts widths from a fresh look at the panes, without a full reconcile.
    pub(super) async fn check_widths(&mut self) {
        match self.opts.tmux.snapshot().await {
            Ok(snap) => self.reassert_widths(&snap.panes).await,
            Err(e) => log!("failed to read panes to check sidebar widths: {e:#}"),
        }
    }

    pub(super) fn windows(&self) -> HashSet<String> {
        self.snapshot
            .panes
            .iter()
            .map(|p| p.window_id.clone())
            .collect()
    }

    pub(super) fn windows_with_sidebar(&self) -> HashSet<String> {
        self.snapshot
            .panes
            .iter()
            .filter(|p| p.is_sidebar)
            .map(|p| p.window_id.clone())
            .collect()
    }

    /// Windows holding a restored sidebar not yet adopted, which need no new one.
    fn windows_with_restored_sidebar(&self) -> HashSet<String> {
        self.snapshot
            .panes
            .iter()
            .filter(|p| p.restored_sidebar)
            .map(|p| p.window_id.clone())
            .collect()
    }

    pub(super) async fn toggle(&mut self) -> bool {
        self.reconcile().await;
        if self.sidebar.on {
            self.sidebar.on = false;
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
            self.show_sidebars().await;
        }
        self.sidebar.known_windows = self.windows();
        self.publish();
        self.schedule_reconcile();
        self.sidebar.on
    }

    /// Turns the sidebar on: a sidebar in every window that has none, the width hook, and, if
    /// configured, the cut-down status line.
    pub(super) async fn show_sidebars(&mut self) {
        self.sidebar.on = true;
        let mut have = self.windows_with_sidebar();
        have.extend(self.windows_with_restored_sidebar());
        for window in self.windows().difference(&have) {
            self.add_sidebar(window).await;
        }
        // The snapshot doesn't show the sidebars just added, so the next reconcile mustn't
        // take their windows for new ones.
        self.sidebar.known_windows = self.windows();
        self.set_layout_hook(true).await;
        if self.opts.config.sidebar.hide_status
            && let Err(e) = self.hide_status().await
        {
            log!("failed to hide the status line: {e:#}");
        }
    }

    /// Restarts as sidebars the panes a session restore brought back in their place (see
    /// `PaneInfo::restored_sidebar`), and marks them as sidebars in `snap`. Returns whether any
    /// were adopted.
    ///
    /// A slot alone in its window waits: the restore adds the window's other panes after it,
    /// and a lone sidebar is closed, which would close the window under the restore. A window
    /// keeps at most one sidebar, so a slot beside another stays a plain pane.
    pub(super) async fn adopt_restored_sidebars(&self, snap: &mut Snapshot) -> bool {
        let cmd = self.sidebar_command();
        let build = self.opts.identity.tag();
        let mut have: HashSet<String> = snap
            .panes
            .iter()
            .filter(|p| p.is_sidebar)
            .map(|p| p.window_id.clone())
            .collect();
        let mut adopted = false;
        for pane in snap.panes.iter_mut() {
            if !pane.restored_sidebar || pane.window_panes < 2 || have.contains(&pane.window_id) {
                continue;
            }
            let id = pane.id.clone();
            let mut cmds = vec![vec!["respawn-pane", "-k", "-t", &id, &cmd]];
            cmds.extend(sidebar_marks(&id, &build));
            // The restore's layout changes fired while this was a plain pane; rerun the user's
            // hooks so any that skip sidebars see it, as `add_sidebar` does.
            cmds.push(vec!["set-hook", "-R", "-t", &id, LAYOUT_HOOK]);
            if pane.active {
                // The sidebar lies left of the window's other panes.
                cmds.push(vec!["select-pane", "-R", "-t", &id]);
            }
            // One tmux call, which stops at a failed command, so a sidebar never runs untagged
            // and gets restarted by every later reconcile.
            if let Err(e) = self.opts.tmux.run_async(&chain(cmds)).await {
                log!("failed to restart restored sidebar {id}: {e:#}");
                continue;
            }
            log!("adopted restored sidebar {id}");
            pane.is_sidebar = true;
            pane.restored_sidebar = false;
            have.insert(pane.window_id.clone());
            adopted = true;
        }
        adopted
    }

    /// Folds or unfolds `session` in every sidebar. tmux sends no notification for an option
    /// change, so this publishes the change itself.
    pub(super) async fn set_collapsed(&mut self, session: &str, collapsed: bool) -> Result<()> {
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
            self.sidebar.collapsed.insert(session.to_owned());
        } else {
            self.sidebar.collapsed.remove(session);
        }
        self.publish();
        Ok(())
    }

    /// The status-format[0] that hide_status replaced. It lives in tmux so a daemon that
    /// adopts sidebars after a restart can still restore it.
    pub(super) async fn saved_status(&self) -> Result<Option<String>> {
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
    pub(super) async fn hide_status(&self) -> Result<()> {
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

    pub(super) async fn restore_status(&self) -> Result<()> {
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
    pub(super) async fn maintain_sidebars(&mut self) {
        let windows = self.windows();
        if self.sidebar.on {
            let mut have = self.windows_with_sidebar();
            // Adopted once the restore gives its window a second pane.
            have.extend(self.windows_with_restored_sidebar());
            let new: Vec<String> = windows
                .difference(&self.sidebar.known_windows)
                .filter(|w| !have.contains(*w))
                .cloned()
                .collect();
            for window in &new {
                self.add_sidebar(window).await;
            }
        }
        self.sidebar.known_windows = windows;
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
    pub(super) fn sidebar_command(&self) -> String {
        format!(
            "SIMUL_SOCKET={} exec {} sidebar",
            shell_quote(&self.opts.socket.to_string_lossy()),
            shell_quote(&self.opts.exe.to_string_lossy()),
        )
    }

    /// Restarts in place each sidebar left by another build, such as the daemon this one
    /// replaced, so the sidebar runs this build's code too.
    pub(super) async fn restart_stale_sidebars(&self) {
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
            let respawn = vec!["respawn-pane", "-k", "-t", pane, &cmd];
            let cmds = std::iter::once(respawn).chain(sidebar_marks(pane, &tag));
            if let Err(e) = self.opts.tmux.run_async(&chain(cmds)).await {
                log!("failed to restart sidebar {pane}: {e:#}");
            }
        }
    }

    pub(super) async fn add_sidebar(&self, window: &str) {
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
        // untagged sidebar and adds a second one. The split fired window-layout-changed before
        // the tag existed; the rerun lets the user's hooks that skip sidebars (e.g. pane-count
        // checks) see it. It also reruns simul's own width hook, which is cheap: the width is
        // already right.
        let build = self.opts.identity.tag();
        let rerun = vec!["set-hook", "-R", "-t", &pane, LAYOUT_HOOK];
        let cmds = sidebar_marks(&pane, &build).into_iter().chain([rerun]);
        if let Err(e) = self.opts.tmux.run_async(&chain(cmds)).await {
            log!("failed to tag sidebar {pane}: {e:#}");
        }
    }

    /// Brings every sidebar in `panes` back to its share of the window (see `width::resizes`).
    pub(super) async fn reassert_widths(&mut self, panes: &[PaneInfo]) {
        for (pane, cols) in width::resizes(panes, &mut self.sidebar.width_requested) {
            let cols = cols.to_string();
            let resize = ["resize-pane", "-t", &pane, "-x", &cols];
            if let Err(e) = self.opts.tmux.run_async(&resize).await {
                log!("failed to resize sidebar {pane} to {cols} columns: {e:#}");
            }
        }
    }
}
