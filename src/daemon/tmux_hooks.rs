//! The tmux hooks the daemon installs to hear about focus and layout changes that control mode
//! doesn't report.

use super::{Daemon, HOOK_INDEX, LAYOUT_HOOK};
use crate::log;
use crate::tmux::{chain, shell_quote, tmux_double_quote};

/// The hooks set for the daemon's whole life, and the `simul internal` subcommand each runs.
/// `LAYOUT_HOOK` comes and goes with the sidebars.
const HOOKS: [(&str, &str); 3] = [
    ("pane-focus-in", "refresh"),
    ("pane-focus-out", "refresh"),
    ("client-resized", "resized"),
];

impl Daemon {
    /// tmux sends no control-mode notification for terminal focus, nor `%layout-change` for
    /// windows outside the control client's session, so hooks poke the daemon.
    pub(super) async fn install_tmux_hooks(&mut self) {
        for (hook, sub) in HOOKS {
            let name = format!("{hook}[{HOOK_INDEX}]");
            let run = self.hook_command(sub);
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

    /// Unsets the hooks that still call this daemon, so none is left calling a daemon that
    /// is gone. Another daemon on the same tmux server may have written the same entries
    /// since; those stay. Two tmux calls, so a slow tmux leaves all or none of them.
    pub(super) async fn remove_tmux_hooks(&self) {
        let hooks = HOOKS.iter().map(|(hook, _)| *hook).chain([LAYOUT_HOOK]);
        let show = chain(hooks.map(|hook| vec!["show-hooks", "-g", hook]));
        let listing = match self.opts.tmux.run_async(&show).await {
            Ok(l) => l,
            Err(e) => {
                log!("failed to read tmux hooks, leaving them in place: {e:#}");
                return;
            }
        };
        let ours = ours_in(&listing, &self.opts.socket.to_string_lossy());
        if ours.is_empty() {
            return;
        }
        let unset = chain(
            ours.iter()
                .map(|name| vec!["set-hook", "-gu", name.as_str()]),
        );
        if let Err(e) = self.opts.tmux.run_async(&unset).await {
            log!("failed to remove tmux hooks {}: {e:#}", ours.join(", "));
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

/// The `hook[HOOK_INDEX]` entries in a `show-hooks` listing that run against `socket`.
fn ours_in(listing: &str, socket: &str) -> Vec<String> {
    let index = format!("[{HOOK_INDEX}]");
    let marker = format!("SIMUL_SOCKET={} ", shell_quote(socket));
    listing
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(name, body)| name.ends_with(&index) && body.contains(&marker))
        .map(|(name, _)| name.to_owned())
        .collect()
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
    fn only_this_daemons_hooks_are_ours() {
        let mine = hook_shell("/run/a b/s.sock", "/x/simul", "refresh");
        let theirs = hook_shell("/run/other/s.sock", "/x/simul", "resized");
        let listing = format!(
            "pane-focus-in[7463] run-shell -b \"{mine}\"\n\
             pane-focus-in[0] run-shell -b \"{mine}\"\n\
             client-resized[7463] run-shell -b \"{theirs}\"\n\
             window-layout-changed[7463] run-shell -b \"{mine}\"\n"
        );
        assert_eq!(
            ours_in(&listing, "/run/a b/s.sock"),
            ["pane-focus-in[7463]", "window-layout-changed[7463]"]
        );
    }

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
