use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use simul::protocol::{Ack, Request, Toggled};
use simul::{cli, client, config, daemon, paths, tmux::Tmux};

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
    /// Install simul's hooks into an agent's settings.
    InstallHooks {
        #[arg(value_parser = ["claude", "kiro"])]
        agent: String,
        /// Install for this directory only: ./.claude/settings.local.json for Claude,
        /// ./.kiro/hooks/simul.json (excluded from git) for Kiro.
        #[arg(long)]
        project: bool,
        /// Install into this file (Claude settings, or the Kiro hook file).
        #[arg(long, conflicts_with = "project")]
        settings: Option<PathBuf>,
    },
    /// Check the tmux setup, daemon, and installed hooks.
    Doctor,
    /// Show or hide the sidebar in every window.
    Toggle,
    /// Run the sidebar UI in the current pane (opened by `toggle`).
    Sidebar,
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
    Resized,
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
        Command::InstallHooks {
            agent,
            project,
            settings,
        } => cli::install_hooks::run(&agent, project, settings),
        Command::Doctor => cli::doctor::run(),
        Command::Toggle => {
            let _: Toggled = client::request(&paths::client_socket()?, &Request::Toggle)?;
            Ok(())
        }
        Command::Sidebar => simul::sidebar::run(),
        Command::Daemon { tmux_socket } => run_daemon(tmux_socket),
        Command::Internal { what } => {
            let req = match what {
                Internal::Refresh => Request::Refresh,
                Internal::Resized => Request::Resized,
            };
            let _: Ack = client::request(&paths::client_socket()?, &req)?;
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
        paths::ensure_private_dir(dir)?;
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
