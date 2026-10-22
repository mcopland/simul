use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::Cmd;
use crate::log;
use crate::protocol::{Ack, Request, Toggled};

/// Binds the daemon socket, or returns `None` if a live daemon already owns it.
pub fn bind(path: &Path) -> Result<Option<UnixListener>> {
    if let Some(dir) = path.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    match UnixListener::bind(path) {
        Ok(l) => Ok(Some(l)),
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Ok(None);
            }
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove stale socket: {}", path.display()))?;
            UnixListener::bind(path)
                .map(Some)
                .with_context(|| format!("failed to bind socket: {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("failed to bind socket: {}", path.display())),
    }
}

pub fn remove_socket(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        log!("failed to remove socket {}: {e}", path.display());
    }
}

pub async fn accept(listener: UnixListener, tx: mpsc::Sender<Cmd>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(stream, tx).await {
                        log!("client connection: {e:#}");
                    }
                });
            }
            Err(e) => log!("accept failed: {e}"),
        }
    }
}

async fn write_json(w: &mut OwnedWriteHalf, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_string(value).context("failed to encode reply")?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .context("failed to write reply")
}

async fn serve(stream: UnixStream, tx: mpsc::Sender<Cmd>) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await.context("failed to read request")? {
        let req: Request =
            serde_json::from_str(&line).with_context(|| format!("malformed request: {line}"))?;
        match req {
            Request::Hook(env) => send(&tx, Cmd::Hook(env)).await?,
            Request::List => {
                let (reply, rx) = oneshot::channel();
                send(&tx, Cmd::List(reply)).await?;
                write_json(&mut w, &rx.await.context("daemon dropped list request")?).await?;
            }
            Request::Subscribe => {
                let (reply, rx) = oneshot::channel();
                send(&tx, Cmd::Subscribe(reply)).await?;
                let (status, mut updates) = rx.await.context("daemon dropped subscribe request")?;
                write_json(&mut w, &status).await?;
                loop {
                    match updates.recv().await {
                        Ok(status) => write_json(&mut w, &status).await?,
                        // A slow reader only needs the latest state, which the next send carries.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
            Request::Ping => write_json(&mut w, &Ack { ok: true }).await?,
            Request::Toggle => {
                let (reply, rx) = oneshot::channel();
                send(&tx, Cmd::Toggle(reply)).await?;
                let sidebar = rx.await.context("daemon dropped toggle request")?;
                write_json(&mut w, &Toggled { sidebar }).await?;
            }
            Request::Resized => {
                send(&tx, Cmd::Resized).await?;
                write_json(&mut w, &Ack { ok: true }).await?;
            }
            Request::Refresh => {
                send(&tx, Cmd::Refresh).await?;
                write_json(&mut w, &Ack { ok: true }).await?;
            }
        }
    }
    Ok(())
}

async fn send(tx: &mpsc::Sender<Cmd>, cmd: Cmd) -> Result<()> {
    tx.send(cmd)
        .await
        .map_err(|_| anyhow::anyhow!("daemon is shutting down"))
}
