//! The tmux hooks the daemon installs to hear about focus and layout changes that control mode
//! doesn't report.

use super::{Daemon, HOOK_INDEX, LAYOUT_HOOK};

/// Every hook the daemon may set, at `HOOK_INDEX`.
const HOOKS: [&str; 4] = [
    "pane-focus-in",
    "pane-focus-out",
    "client-resized",
    LAYOUT_HOOK,
];
use crate::log;
use crate::tmux::{shell_quote, tmux_double_quote};

impl Daemon {
    pub(super) async fn query_focus_events(&self) -> bool {
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
    pub(super) async fn install_tmux_hooks(&mut self) {
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

    /// Unsets every hook this daemon set, so none is left calling a daemon that is gone.
    pub(super) async fn remove_tmux_hooks(&self) {
        for hook in HOOKS {
            let name = format!("{hook}[{HOOK_INDEX}]");
            if let Err(e) = self.opts.tmux.run_async(&["set-hook", "-gu", &name]).await {
                log!("failed to remove tmux hook {name}: {e:#}");
            }
        }
    }

    /// A hook body that runs `simul internal <sub>` against this daemon.
    pub(super) fn hook_command(&self, sub: &str) -> String {
        let cmd = hook_shell(
            &self.opts.socket.to_string_lossy(),
            &self.opts.exe.to_string_lossy(),
            sub,
        );
        format!("run-shell -b {}", tmux_double_quote(&cmd))
    }

    /// Installs, or removes, the hook that reports a dragged sidebar border in any session.
    /// It fires on every layout change, so it is only set while sidebars are shown.
    pub(super) async fn set_layout_hook(&self, on: bool) {
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
}

/// The shell line a hook runs. It always succeeds: tmux shows a failed `run-shell` in the
/// active pane, and the hook outlives a daemon that crashed or a binary that was deleted.
fn hook_shell(socket: &str, exe: &str, sub: &str) -> String {
    format!(
        "SIMUL_SOCKET={} {} internal {sub} >/dev/null 2>&1 || true",
        shell_quote(socket),
        shell_quote(exe),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_shell_succeeds_without_the_binary() {
        let line = hook_shell("/nonexistent/s.sock", "/nonexistent/simul", "refresh");
        let status = std::process::Command::new("sh")
            .args(["-c", &line])
            .status()
            .unwrap();
        assert!(status.success(), "{line:?} exited {status:?}");
    }
}
