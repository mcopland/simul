use anyhow::Result;

use crate::hook::{Agent, Screen};
use crate::tmux::Tmux;

/// Captures the visible pane and classifies it with the agent's recorded screen signatures.
pub async fn screen(tmux: &Tmux, pane: &str, agent: Agent) -> Result<Screen> {
    let text = tmux
        .run_async(&["capture-pane", "-p", "-J", "-t", pane])
        .await?;
    Ok(agent.classify_screen(&text))
}
