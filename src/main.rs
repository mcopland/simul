use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use simul::{cli, config, daemon, paths, tmux::Tmux};

#[derive(Parser)]
#[command(name = "simul", version, about = "Track coding agents running in tmux")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List tracked agents.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Run the daemon for one tmux server (normally started automatically).
    #[command(hide = true)]
    Daemon {
        /// tmux server socket; defaults to the one in $TMUX.
        #[arg(long)]
        tmux_socket: Option<PathBuf>,
    },
    /// Called by tmux hooks.
    #[command(hide = true)]
    Internal {
        #[command(subcommand)]
        what: Internal,
    },
}

#[derive(Subcommand)]
enum Internal {
    Refresh,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Hooks bypass clap: a usage error must still exit 0 without printing to stdout.
    if args.get(1).map(String::as_str) == Some("hook") {
        simul::hook::shim::run(&args[2..]);
    }
    if let Err(e) = run(Cli::parse()) {
        eprintln!("simul: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Ls { json } => cli::ls::run(json),
        Command::Daemon { tmux_socket } => run_daemon(tmux_socket),
        Command::Internal { what } => {
            match what {
                Internal::Refresh => {
                    let _: simul::protocol::Ack = simul::client::request(
                        &paths::client_socket()?,
                        &simul::protocol::Request::Refresh,
                    )?;
                }
            }
            Ok(())
        }
    }
}

fn run_daemon(tmux_socket: Option<PathBuf>) -> Result<()> {
    let tmux_socket = tmux_socket
        .or_else(paths::current_tmux_socket)
        .context("daemon needs a tmux server: pass --tmux-socket or run inside tmux")?;
    let socket = paths::socket_path(paths::env_var, paths::uid(), Some(&tmux_socket))?;
    let log_path = socket.with_extension("log");
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    simul::log::init(&log_path)?;
    let opts = daemon::Options {
        socket,
        tmux: Tmux::new(tmux_socket),
        exe: std::env::current_exe().context("failed to locate the simul binary")?,
        config: config::load()?,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?
        .block_on(daemon::run(opts))
}
