use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use super::Cmd;
use crate::log;
use crate::tmux::Tmux;

/// One line of `tmux -C` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    /// `%begin`/`%end`/`%error` framing and the command output between them.
    Reply,
    Notification {
        name: String,
        args: Vec<String>,
    },
    Exit,
}

/// Tracks `%begin`/`%end` framing so command output that starts with `%` is not taken for a
/// notification.
#[derive(Debug, Default)]
pub struct Parser {
    in_reply: bool,
}

impl Parser {
    pub fn feed(&mut self, line: &str) -> Line {
        if line.starts_with("%begin") {
            self.in_reply = true;
            return Line::Reply;
        }
        if line.starts_with("%end") || line.starts_with("%error") {
            self.in_reply = false;
            return Line::Reply;
        }
        if self.in_reply || !line.starts_with('%') {
            return Line::Reply;
        }
        let mut words = line.split(' ');
        let name = words.next().unwrap_or_default();
        if name == "%exit" {
            return Line::Exit;
        }
        Line::Notification {
            name: name.to_owned(),
            args: words.map(str::to_owned).collect(),
        }
    }
}

/// What simul's tmux hooks ask of the daemon, sent as `display-message -c <this client>`
/// (see `tmux_hooks::hook_body`) and read here from `%message simul <what>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poke {
    /// Focus may have moved.
    Refresh,
    /// A client or a layout was resized.
    Resized,
}

impl Poke {
    pub fn word(self) -> &'static str {
        match self {
            Poke::Refresh => "refresh",
            Poke::Resized => "resized",
        }
    }
}

/// The word that starts each of simul's hook messages.
pub const MESSAGE_PREFIX: &str = "simul";

/// The poke a notification carries, if it is one of simul's hook messages.
pub fn poke(name: &str, args: &[String]) -> Option<Poke> {
    if name != "%message" {
        return None;
    }
    match args {
        [prefix, what] if prefix == MESSAGE_PREFIX => [Poke::Refresh, Poke::Resized]
            .into_iter()
            .find(|p| p.word() == what),
        _ => None,
    }
}

/// The name of the client whose process is `pid` in `list-clients -F CLIENT_PIDS` output.
fn client_named(listing: &str, pid: u32) -> Option<String> {
    listing.lines().find_map(|line| {
        let (p, name) = line.split_once('\t')?;
        (p.parse() == Ok(pid)).then(|| name.to_owned())
    })
}

const CLIENT_PIDS: &str = "#{client_pid}\t#{client_name}";

/// Notifications after which pane, window, or focus state may have changed.
pub fn triggers_reconcile(name: &str) -> bool {
    matches!(
        name,
        "%window-add"
            | "%unlinked-window-add"
            | "%window-close"
            | "%unlinked-window-close"
            | "%layout-change"
            | "%window-pane-changed"
            | "%session-window-changed"
            | "%client-session-changed"
            | "%sessions-changed"
            | "%session-changed"
            | "%client-detached"
            | "%window-renamed"
            | "%unlinked-window-renamed"
            | "%session-renamed"
    )
}

/// Streams tmux notifications to the daemon, reattaching while the server lives.
///
/// The control client attaches to one session, and tmux detaches it when that session is
/// destroyed, so EOF alone does not mean the server is gone.
pub async fn run(tmux: Tmux, tx: mpsc::Sender<Cmd>) {
    loop {
        if let Err(e) = attach_once(&tmux, &tx).await {
            log!("control mode: {e:#}");
        }
        if tx.is_closed() || tmux.run_async(&["list-sessions"]).await.is_err() {
            // The receiver closing means the daemon already stopped.
            let _ = tx.send(Cmd::Shutdown(super::Exit::TmuxGone)).await;
            return;
        }
        // Reattached: state may have changed while detached. A send error means shutdown.
        let _ = tx
            .send(Cmd::TmuxNotification {
                name: "%sessions-changed".into(),
            })
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn attach_once(tmux: &Tmux, tx: &mpsc::Sender<Cmd>) -> Result<()> {
    let mut child = tokio::process::Command::new("tmux")
        .arg("-S")
        .arg(tmux.socket())
        .args(["-C", "attach", "-f", "no-output,ignore-size"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start tmux control mode")?;
    // Control mode exits when its stdin closes, so hold it open for the client's lifetime.
    let _stdin = child.stdin.take();
    let pid = child.id().context("tmux control mode exited at once")?;
    // Hooks reach the daemon through this client by name, and a new attach has a new name.
    let name = find_client(tmux, pid).await?;
    if tx.send(Cmd::ControlClient(name)).await.is_err() {
        return Ok(());
    }
    let stdout = child
        .stdout
        .take()
        .context("tmux control mode has no stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut parser = Parser::default();
    while let Some(line) = lines
        .next_line()
        .await
        .context("failed to read tmux control mode output")?
    {
        match parser.feed(&line) {
            Line::Notification { name, args } => {
                let cmd = match poke(&name, &args) {
                    Some(Poke::Refresh) => Cmd::Refresh,
                    Some(Poke::Resized) => Cmd::Resized,
                    None => Cmd::TmuxNotification { name },
                };
                if tx.send(cmd).await.is_err() {
                    return Ok(());
                }
            }
            Line::Exit => break,
            Line::Reply => {}
        }
    }
    Ok(())
}

/// The name tmux gives the control client running as `pid`, once it has registered.
async fn find_client(tmux: &Tmux, pid: u32) -> Result<String> {
    const TRIES: u32 = 50;
    for _ in 0..TRIES {
        let listing = tmux.run_async(&["list-clients", "-F", CLIENT_PIDS]).await?;
        if let Some(name) = client_named(&listing, pid) {
            return Ok(name);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("tmux never listed control mode client {pid} in list-clients")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(name: &str, args: &[&str]) -> Line {
        Line::Notification {
            name: name.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parses_notifications_and_framing() {
        let mut p = Parser::default();
        assert_eq!(p.feed("%begin 1 2 0"), Line::Reply);
        assert_eq!(p.feed("%window-add @9"), Line::Reply);
        assert_eq!(p.feed("%end 1 2 0"), Line::Reply);
        assert_eq!(p.feed("%window-add @9"), note("%window-add", &["@9"]));
        assert_eq!(
            p.feed("%window-pane-changed @1 %3"),
            note("%window-pane-changed", &["@1", "%3"])
        );
        assert_eq!(p.feed("%layout-change"), note("%layout-change", &[]));
        assert_eq!(p.feed("%begin 3 4 0"), Line::Reply);
        assert_eq!(p.feed("%error 3 4 0"), Line::Reply);
        assert_eq!(p.feed("%exit"), Line::Exit);
        assert_eq!(p.feed("%exit server exited"), Line::Exit);
        assert_eq!(p.feed("stray"), Line::Reply);
    }

    #[test]
    fn simuls_messages_poke_the_daemon() {
        let words = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            poke("%message", &words("simul refresh")),
            Some(Poke::Refresh)
        );
        assert_eq!(
            poke("%message", &words("simul resized")),
            Some(Poke::Resized)
        );
        assert_eq!(poke("%message", &words("hello there")), None);
        assert_eq!(poke("%message", &words("simul other")), None);
        assert_eq!(poke("%window-add", &words("simul refresh")), None);
    }

    #[test]
    fn the_control_client_is_found_by_its_pid() {
        let listing = "41\tclient-41\n4242\tclient-4242\n";
        assert_eq!(client_named(listing, 4242).as_deref(), Some("client-4242"));
        assert_eq!(client_named(listing, 7), None);
    }

    #[test]
    fn reconcile_triggers() {
        for name in [
            "%window-add",
            "%unlinked-window-add",
            "%window-close",
            "%unlinked-window-close",
            "%layout-change",
            "%window-pane-changed",
            "%session-window-changed",
            "%client-session-changed",
            "%sessions-changed",
            "%session-changed",
            "%client-detached",
            "%window-renamed",
            "%unlinked-window-renamed",
            "%session-renamed",
        ] {
            assert!(triggers_reconcile(name), "{name}");
        }
        assert!(!triggers_reconcile("%output"));
    }
}
