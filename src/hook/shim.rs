//! `simul hook <agent> <event>`, run by the agent for every hook.
//!
//! Agents feed hook stdout into the model's context, and some treat a non-zero exit as a
//! veto, so this never writes to stdout, always exits 0, and gives up after a fixed budget.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::{Envelope, slim_payload};
use crate::paths::{self, env_var};
use crate::protocol::Request;

const BUDGET: Duration = Duration::from_millis(200);
const MAX_PAYLOAD: u64 = 16 * 1024 * 1024;

pub fn run(args: &[String]) -> ! {
    let deadline = Instant::now() + BUDGET;
    let args = args.to_vec();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| deliver(&args, deadline));
        // The receiver is gone only if the budget already ran out; nothing left to report to.
        let _ = tx.send(result);
    });
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => eprintln!("simul hook: {e:#}"),
        Ok(Err(_)) => eprintln!("simul hook: panicked"),
        Err(_) => eprintln!("simul hook: gave up after {}ms", BUDGET.as_millis()),
    }
    std::process::exit(0)
}

fn deliver(args: &[String], deadline: Instant) -> Result<()> {
    let [agent, event, ..] = args else {
        bail!("usage: simul hook <agent> <event>");
    };
    let mut raw = String::new();
    std::io::stdin()
        .take(MAX_PAYLOAD)
        .read_to_string(&mut raw)
        .context("failed to read hook payload from stdin")?;
    let Some(pane) = env_var("TMUX_PANE") else {
        // Not inside tmux: nothing to track.
        return Ok(());
    };
    // A malformed payload still carries the event name, which is enough to update state.
    let payload = serde_json::from_str::<Value>(&raw).unwrap_or_else(|e| {
        eprintln!("simul hook: payload is not JSON ({e}); sending event without it");
        Value::Null
    });
    let req = Request::Hook(Envelope {
        agent: agent.clone(),
        event: event.clone(),
        pane: Some(pane),
        tmux: env_var("TMUX"),
        payload: slim_payload(&payload),
    });
    let mut line = serde_json::to_string(&req).context("failed to encode hook envelope")?;
    line.push('\n');

    let tmux_socket = paths::current_tmux_socket();
    let sockets = paths::socket_candidates(env_var, paths::uid(), tmux_socket.as_deref())?;
    let mut stream = connect_or_start(&sockets, deadline)?;
    stream
        .write_all(line.as_bytes())
        .context("failed to write hook to the simul daemon")
}

/// Connects to the first live daemon socket whose directory is private to this user.
fn connect_any(sockets: &[PathBuf]) -> Option<UnixStream> {
    sockets.iter().find_map(|s| {
        let private = s
            .parent()
            .is_none_or(|d| paths::check_private_dir(d).is_ok());
        private.then(|| UnixStream::connect(s).ok()).flatten()
    })
}

fn connect_or_start(sockets: &[PathBuf], deadline: Instant) -> Result<UnixStream> {
    if let Some(stream) = connect_any(sockets) {
        return Ok(stream);
    }
    start_daemon()?;
    loop {
        std::thread::sleep(Duration::from_millis(10));
        if let Some(stream) = connect_any(sockets) {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            bail!("daemon did not come up at {}", sockets[0].display());
        }
    }
}

fn start_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate the simul binary")?;
    Command::new(&exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Own process group so the agent killing the hook's group does not take the daemon.
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to start daemon: {}", exe.display()))?;
    Ok(())
}
