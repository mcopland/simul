//! `simul hook <agent> <event>`, run by the agent for every hook.
//!
//! Agents feed hook stdout into the model's context, and some treat a non-zero exit as a
//! veto, so this never writes to stdout, always exits 0, and gives up after a fixed budget.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
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
    let payload = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
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
    let socket = paths::socket_path(env_var, paths::uid(), tmux_socket.as_deref())?;
    let mut stream = connect_or_start(&socket, deadline)?;
    stream
        .write_all(line.as_bytes())
        .with_context(|| format!("failed to write hook to {}", socket.display()))
}

fn connect_or_start(socket: &Path, deadline: Instant) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    start_daemon()?;
    loop {
        std::thread::sleep(Duration::from_millis(10));
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            bail!("daemon did not come up at {}", socket.display());
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
