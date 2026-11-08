//! The tmux hooks the daemon installs to hear about focus and layout changes that control mode
//! doesn't report.

use super::{Daemon, HOOK_INDEX, LAYOUT_HOOK};
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

    /// A hook body that runs `simul internal <sub>` against this daemon.
    pub(super) fn hook_command(&self, sub: &str) -> String {
        let cmd = format!(
            "SIMUL_SOCKET={} {} internal {sub} >/dev/null 2>&1",
            shell_quote(&self.opts.socket.to_string_lossy()),
            shell_quote(&self.opts.exe.to_string_lossy()),
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
