use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    /// tmux refused a command this client sent, with the error's output lines.
    Rejected(Vec<String>),
    Exit,
}

/// Tracks `%begin`/`%end` framing so command output that starts with `%` is not taken for a
/// notification.
#[derive(Debug, Default)]
pub struct Parser {
    in_reply: bool,
    /// The output of the reply being read, when it answers this client's own command.
    ours: Option<Vec<String>>,
}

/// Whether a `%begin`/`%end`/`%error` line's flags say the command came from this client.
fn from_this_client(line: &str) -> bool {
    line.split(' ').nth(3) == Some("1")
}

impl Parser {
    pub fn feed(&mut self, line: &str) -> Line {
        if line.starts_with("%begin") {
            self.in_reply = true;
            self.ours = from_this_client(line).then(Vec::new);
            return Line::Reply;
        }
        if line.starts_with("%end") || line.starts_with("%error") {
            self.in_reply = false;
            let ours = self.ours.take();
            return match ours {
                Some(output) if line.starts_with("%error") && from_this_client(line) => {
                    Line::Rejected(output)
                }
                _ => Line::Reply,
            };
        }
        if self.in_reply {
            if let Some(output) = &mut self.ours {
                output.push(line.to_owned());
            }
            return Line::Reply;
        }
        if !line.starts_with('%') {
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
    /// A shell marked a command's start or end with OSC 133.
    Command,
}

impl Poke {
    pub fn word(self) -> &'static str {
        match self {
            Poke::Refresh => "refresh",
            Poke::Resized => "resized",
            Poke::Command => "command",
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
        [prefix, what] if prefix == MESSAGE_PREFIX => [Poke::Refresh, Poke::Resized, Poke::Command]
            .into_iter()
            .find(|p| p.word() == what),
        _ => None,
    }
}

/// The name of simul's tmux subscription.
pub const SUBSCRIPTION: &str = "simul-panes";

/// What tmux checks once a second for simul, reporting each change with
/// `%subscription-changed`: what no notification covers. That is each pane's foreground
/// command and directory, window indexes (renumbering sends nothing), zoom in windows of
/// sessions this client isn't attached to, and which clients show which session with what
/// focus. The `S:` loop reaches every session, though the subscription is the attached
/// session's.
pub const PANES_SUBSCRIPTION: &str = "#{L:#{client_name}=#{client_session}=#{client_flags};}|\
#{S:#{W:#{window_id}=#{window_index}=#{window_zoomed_flag}:#{P:#{pane_id}=#{pane_current_command}=#{pane_current_path};}}}";

/// The control-mode command that subscribes this client to `PANES_SUBSCRIPTION`.
pub fn subscribe_command() -> String {
    format!("refresh-client -B '{SUBSCRIPTION}::{PANES_SUBSCRIPTION}'\n")
}

/// Whether a notification is tmux reporting a change to simul's subscription.
pub fn panes_changed(name: &str, args: &[String]) -> bool {
    name == "%subscription-changed" && args.first().is_some_and(|s| s == SUBSCRIPTION)
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

/// `tmux` arguments for the control client. `attach` may start a server, which loads the
/// user's config and can restore sessions into it; `-N` keeps a daemon that outlived its
/// server from starting a new one.
fn attach_args(socket: &Path) -> Vec<OsString> {
    let mut args = vec![OsString::from("-S"), socket.as_os_str().to_owned()];
    args.extend(["-N", "-C", "attach", "-f", "no-output,ignore-size"].map(OsString::from));
    args
}

async fn attach_once(tmux: &Tmux, tx: &mpsc::Sender<Cmd>) -> Result<()> {
    let mut child = tokio::process::Command::new("tmux")
        .args(attach_args(tmux.socket()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start tmux control mode")?;
    // Control mode exits when its stdin closes, so hold it open for the client's lifetime.
    let mut stdin = child
        .stdin
        .take()
        .context("tmux control mode has no stdin")?;
    stdin
        .write_all(subscribe_command().as_bytes())
        .await
        .context("failed to subscribe tmux control mode to pane changes")?;
    let pid = child.id().context("tmux control mode exited at once")?;
    // Hooks reach the daemon through this client by name, and a new attach has a new name.
    let name = find_client(tmux, pid).await?;
    if tx.send(Cmd::ControlClient { name, pid }).await.is_err() {
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
                    Some(Poke::Command) => Cmd::ShellCommand,
                    None if panes_changed(&name, &args) => Cmd::PanesChanged,
                    None => Cmd::TmuxNotification { name },
                };
                if tx.send(cmd).await.is_err() {
                    return Ok(());
                }
            }
            Line::Rejected(output) => {
                // The only command this client sends is the subscription.
                log!(
                    "tmux rejected simul's pane subscription ({}); checking panes every \
                     second instead",
                    output.join("; ")
                );
                if tx.send(Cmd::SubscriptionFailed).await.is_err() {
                    return Ok(());
                }
            }
            Line::Exit => break,
            Line::Reply => {}
        }
    }
    Ok(())
}

/// Kills and reaps the control client `pid` before the daemon `exec`s itself: the new image
/// knows nothing of the old one's children, so the client would stay a zombie for good.
/// Only acts on a child of this process that has not been reaped, so a pid the system has
/// since reused for someone else's process is left alone.
pub fn reap(pid: u32) -> std::io::Result<()> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| std::io::Error::other(format!("control client pid {pid} out of range")))?;
    let mut status = 0;
    // SAFETY: waitpid with WNOHANG only reads this process's own children and writes the
    // exit status into `status`, a valid local.
    match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
        // Still running: ours, and not yet reaped.
        0 => {}
        // It had exited and is now reaped.
        p if p == pid => return Ok(()),
        _ => {
            let e = std::io::Error::last_os_error();
            // Not a child of ours, or reaped already (tokio does when it sees the exit).
            return if e.raw_os_error() == Some(libc::ECHILD) {
                Ok(())
            } else {
                Err(e)
            };
        }
    }
    // SAFETY: `pid` is a live, unreaped child of this process (checked above, and nothing else
    // on the daemon's one thread can reap it meanwhile), so the signal reaches that child.
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above; SIGKILL guarantees the child exits, so this blocking wait returns.
    if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
        return Err(std::io::Error::last_os_error());
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
        assert_eq!(
            poke("%message", &words("simul command")),
            Some(Poke::Command)
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

    #[test]
    fn reap_kills_and_collects_a_running_child() {
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = child.id();
        reap(pid).unwrap();
        // Gone, and already reaped: a second reap finds no child.
        assert!(!crate::claude_sessions::pid_alive(pid as i32));
        reap(pid).unwrap();
        // `child` is not waited on again; dropping it does not reap.
        drop(child);
    }

    #[test]
    fn reap_leaves_processes_that_are_not_its_children() {
        // pid 1 is launchd or init: never a child, so ECHILD, and no signal is sent.
        reap(1).unwrap();
    }

    #[test]
    fn a_rejected_command_of_ours_reports_its_error() {
        let mut p = Parser::default();
        assert_eq!(p.feed("%begin 1 7 1"), Line::Reply);
        assert_eq!(p.feed("unknown format: #{L:"), Line::Reply);
        assert_eq!(
            p.feed("%error 1 7 1"),
            Line::Rejected(vec!["unknown format: #{L:".into()])
        );
        // Another client's command, as the flags say, is not ours to report.
        assert_eq!(p.feed("%begin 2 8 0"), Line::Reply);
        assert_eq!(p.feed("no such session"), Line::Reply);
        assert_eq!(p.feed("%error 2 8 0"), Line::Reply);
        assert_eq!(p.feed("%begin 3 9 1"), Line::Reply);
        assert_eq!(p.feed("%end 3 9 1"), Line::Reply);
    }

    #[test]
    fn only_simuls_subscription_reports_pane_changes() {
        let mut p = Parser::default();
        let Line::Notification { name, args } =
            p.feed("%subscription-changed simul-panes $0 - - - : %0=sh=/tmp;")
        else {
            panic!("not a notification");
        };
        assert!(panes_changed(&name, &args));
        assert!(!panes_changed("%subscription-changed", &["other".into()]));
        assert!(!panes_changed("%window-add", &["simul-panes".into()]));
    }

    #[test]
    fn the_subscription_command_names_simuls_subscription() {
        let cmd = subscribe_command();
        assert!(
            cmd.starts_with(&format!("refresh-client -B '{SUBSCRIPTION}::")),
            "{cmd}"
        );
        assert!(cmd.ends_with("'\n"), "{cmd:?}");
        // Single-quoted for tmux's parser, so the format must hold no quote of its own.
        assert!(!PANES_SUBSCRIPTION.contains('\''));
        for field in [
            "#{pane_current_command}",
            "#{pane_current_path}",
            "#{window_zoomed_flag}",
            "#{window_index}",
            "#{client_flags}",
        ] {
            assert!(PANES_SUBSCRIPTION.contains(field), "{field}");
        }
    }

    #[test]
    fn attaching_never_starts_a_server() {
        let args = attach_args(std::path::Path::new("/tmp/t.sock"));
        let at = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("no {flag} in {args:?}"))
        };
        let (n, c) = (at("-N"), at("-C"));
        assert!(n < c, "-N must come before the command: {args:?}");
        assert_eq!(&args[..2], ["-S", "/tmp/t.sock"]);
    }
}
