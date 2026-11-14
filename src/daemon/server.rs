use std::fs::{File, TryLockError};
use std::io::ErrorKind;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{Cmd, Exit};
use crate::log;
use crate::protocol::{Ack, Request, Toggled};

/// How long a starting daemon waits for another one that holds the socket lock to answer.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Binds the daemon socket, or returns `None` if a live daemon already owns it. The returned
/// file holds the socket lock; keep it open for as long as the socket is served.
pub fn bind(path: &Path) -> Result<Option<(UnixListener, File)>> {
    bind_within(path, LOCK_WAIT)
}

/// The lock file beside the socket at `path`, like the log.
fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

/// Like `bind`, waiting up to `wait` for a lock holder that does not answer yet.
///
/// Taking over a stale socket is check, remove, then bind; two daemons doing it at once could
/// both bind, and one would run on an unlinked socket. The lock lets only one in. Its file is
/// close-on-exec, so a self-update's `exec` frees it for the new image.
fn bind_within(path: &Path, wait: Duration) -> Result<Option<(UnixListener, File)>> {
    if let Some(dir) = path.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    let lock_path = lock_path(path);
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                // A holder that answers is a live daemon; one that doesn't is still starting
                // or just exiting.
                if std::os::unix::net::UnixStream::connect(path).is_ok() {
                    return Ok(None);
                }
                if Instant::now() >= deadline {
                    bail!(
                        "another simul daemon holds {} but does not answer at {} after {wait:?}; stop it with: pkill -f 'simul daemon'",
                        lock_path.display(),
                        path.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("failed to lock {}", lock_path.display()));
            }
        }
    }
    match UnixListener::bind(path) {
        Ok(l) => Ok(Some((l, lock))),
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            // A daemon from before the lock existed may still serve the socket.
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Ok(None);
            }
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove stale socket: {}", path.display()))?;
            UnixListener::bind(path)
                .map(|l| Some((l, lock)))
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
                    match serve(stream, tx).await {
                        Ok(()) => {}
                        Err(e) if is_disconnect(&e) => {}
                        Err(e) => log!("client connection: {e:#}"),
                    }
                });
            }
            Err(e) => log!("accept failed: {e}"),
        }
    }
}

/// A client that hung up (a closed sidebar, an interrupted `ls`) is normal, not an error.
fn is_disconnect(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| {
            matches!(
                io.kind(),
                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
            )
        })
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
            Request::Collapse { session, collapsed } => {
                let (reply, rx) = oneshot::channel();
                send(
                    &tx,
                    Cmd::Collapse {
                        session,
                        collapsed,
                        reply,
                    },
                )
                .await?;
                let ok = rx.await.context("daemon dropped collapse request")?;
                write_json(&mut w, &Ack { ok }).await?;
            }
            Request::Identify => {
                let (reply, rx) = oneshot::channel();
                send(&tx, Cmd::Identify(reply)).await?;
                write_json(
                    &mut w,
                    &rx.await.context("daemon dropped identify request")?,
                )
                .await?;
            }
            Request::Shutdown => {
                // Reply first: once the daemon gets Shutdown, it exits without serving more.
                write_json(&mut w, &Ack { ok: true }).await?;
                send(&tx, Cmd::Shutdown(Exit::Replaced)).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir the daemon accepts for its socket.
    fn private_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    /// A socket file that nothing listens on, as a crashed daemon leaves it.
    fn stale_socket(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("s.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        path
    }

    #[tokio::test]
    async fn a_held_lock_keeps_a_stale_socket_from_a_second_daemon() {
        let dir = private_dir();
        let path = stale_socket(dir.path());
        let holder = std::fs::File::create(lock_path(&path)).unwrap();
        holder.try_lock().unwrap();

        let err = bind_within(&path, Duration::from_millis(100)).unwrap_err();
        assert!(
            format!("{err:#}").contains(&*path.to_string_lossy()),
            "{err:#}"
        );
        assert!(
            path.exists(),
            "the stale socket is the lock holder's to take"
        );

        drop(holder);
        let bound = bind_within(&path, Duration::from_millis(100)).unwrap();
        assert!(
            bound.is_some(),
            "a free lock lets the stale socket be taken"
        );
    }

    #[tokio::test]
    async fn a_live_daemon_keeps_its_socket() {
        let dir = private_dir();
        let path = dir.path().join("s.sock");
        // An older daemon that holds no lock.
        let _live = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(
            bind_within(&path, Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
        // A daemon of this build, which also holds the lock.
        let holder = std::fs::File::create(lock_path(&path)).unwrap();
        holder.try_lock().unwrap();
        assert!(
            bind_within(&path, Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn hangups_are_disconnects() {
        let pipe = anyhow::Error::new(std::io::Error::from(ErrorKind::BrokenPipe))
            .context("failed to write reply");
        assert!(is_disconnect(&pipe));
        let reset = anyhow::Error::new(std::io::Error::from(ErrorKind::ConnectionReset));
        assert!(is_disconnect(&reset));
        let other = anyhow::Error::new(std::io::Error::from(ErrorKind::PermissionDenied));
        assert!(!is_disconnect(&other));
        assert!(!is_disconnect(&anyhow::anyhow!("malformed request")));
    }
}
