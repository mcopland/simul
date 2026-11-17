//! The tmux hooks the daemon installs to hear about focus and layout changes that control mode
//! doesn't report. Each sends the daemon's control client a message, so no process starts.

use super::control_mode::{MESSAGE_PREFIX, Poke};
use super::{Daemon, HOOK_INDEX, LAYOUT_HOOK};
use crate::log;
use crate::tmux::chain;

/// The hooks set while the daemon's control client is attached, and what each asks.
/// `LAYOUT_HOOK` comes and goes with the sidebars. A shell's OSC 133 marks fire the
/// pane-command hooks, so a command's start and end show at once, not at the next check.
const HOOKS: [(&str, Poke); 5] = [
    ("pane-focus-in", Poke::Refresh),
    ("pane-focus-out", Poke::Refresh),
    ("client-resized", Poke::Resized),
    ("pane-command-started", Poke::Command),
    ("pane-command-finished", Poke::Command),
];

impl Daemon {
    /// tmux sends no control-mode notification for terminal focus, nor `%layout-change` for
    /// windows outside the control client's session, so hooks poke the daemon. They name the
    /// control client, so they are set again each time it attaches.
    pub(super) async fn install_tmux_hooks(&mut self) {
        for (hook, poke) in HOOKS {
            let name = format!("{hook}[{HOOK_INDEX}]");
            let Some(run) = self.hook_command(poke) else {
                return;
            };
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

    /// Unsets the hooks that still message this daemon's control client. A hook left behind
    /// would fail silently, but cleanup keeps the hook list honest. Another daemon on the same
    /// tmux server may have written the same entries since; those stay. Two tmux calls, so a
    /// slow tmux leaves all or none of them.
    pub(super) async fn remove_tmux_hooks(&self) {
        if self.control_clients.is_empty() {
            return;
        }
        let hooks = HOOKS.iter().map(|(hook, _)| *hook).chain([LAYOUT_HOOK]);
        let show = chain(hooks.map(|hook| vec!["show-hooks", "-g", hook]));
        let listing = match self.opts.tmux.run_async(&show).await {
            Ok(l) => l,
            Err(e) => {
                log!("failed to read tmux hooks, leaving them in place: {e:#}");
                return;
            }
        };
        let ours = ours_in(&listing, &self.control_clients);
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

    /// A hook body that pokes this daemon; `None` before its control client has attached.
    pub(super) fn hook_command(&self, poke: Poke) -> Option<String> {
        let client = self.control_clients.last()?;
        Some(hook_body(client, poke.word()))
    }

    /// Installs, or removes, the hook that reports a dragged sidebar border in any session.
    /// It fires on every layout change, so it is only set while sidebars are shown.
    /// Before the control client attaches, turning it on waits for `Cmd::ControlClient`.
    pub(super) async fn set_layout_hook(&self, on: bool) {
        let name = format!("{LAYOUT_HOOK}[{HOOK_INDEX}]");
        let run = if on {
            let Some(body) = self.hook_command(Poke::Resized) else {
                return;
            };
            Some(body)
        } else {
            None
        };
        let args = match &run {
            Some(body) => vec!["set-hook", "-g", &name, body],
            None => vec!["set-hook", "-gu", &name],
        };
        if let Err(e) = self.opts.tmux.run_async(&args).await {
            log!(
                "failed to set tmux hook {name} {}: {e:#}",
                if on { "on" } else { "off" }
            );
        }
    }
}

/// The `hook[HOOK_INDEX]` entries in a `show-hooks` listing that message one of `clients`.
fn ours_in(listing: &str, clients: &[String]) -> Vec<String> {
    let index = format!("[{HOOK_INDEX}]");
    let markers: Vec<String> = clients
        .iter()
        .map(|c| format!("display-message -c {c} "))
        .collect();
    listing
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(name, body)| {
            name.ends_with(&index) && markers.iter().any(|m| body.starts_with(m))
        })
        .map(|(name, _)| name.to_owned())
        .collect()
}

/// A hook command that sends `client`, the daemon's control client, `simul <what>`. tmux
/// delivers it as `%message`; for a client that is gone, the hook fails without a word.
fn hook_body(client: &str, what: &str) -> String {
    format!("display-message -c {client} '{MESSAGE_PREFIX} {what}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_messages_the_control_client() {
        assert_eq!(
            hook_body("client-42", "refresh"),
            "display-message -c client-42 'simul refresh'"
        );
    }

    #[test]
    fn only_this_daemons_hooks_are_ours() {
        // As show-hooks prints them.
        let listing = "pane-focus-in[7463] display-message -c client-42 \"simul refresh\"\n\
                       pane-focus-in[0] display-message -c client-42 \"simul refresh\"\n\
                       client-resized[7463] display-message -c client-420 \"simul resized\"\n\
                       window-layout-changed[7463] display-message -c client-42 \"simul resized\"\n\
                       pane-focus-out[7463] run-shell -b \"SIMUL_SOCKET=/s simul internal refresh\"\n";
        assert_eq!(
            ours_in(listing, &["client-42".into()]),
            ["pane-focus-in[7463]", "window-layout-changed[7463]"]
        );
        // A reattach renames the client; a hook that kept the old name is still ours.
        assert_eq!(
            ours_in(listing, &["client-420".into(), "client-42".into()]),
            [
                "pane-focus-in[7463]",
                "client-resized[7463]",
                "window-layout-changed[7463]"
            ]
        );
    }
}
