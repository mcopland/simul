use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

use crate::protocol::{Request, Status};

pub fn connect(socket: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket).with_context(|| {
        format!(
            "failed to connect to the simul daemon at {} (is it running? try `simul doctor`)",
            socket.display()
        )
    })
}

fn send(stream: &mut UnixStream, req: &Request) -> Result<()> {
    let mut line = serde_json::to_string(req).context("failed to encode request")?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .context("failed to send request to the simul daemon")
}

fn read_line<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<T> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .context("failed to read reply from the simul daemon")?
        == 0
    {
        bail!("the simul daemon closed the connection without replying");
    }
    serde_json::from_str(&line).with_context(|| format!("failed to decode daemon reply: {line}"))
}

pub fn request<T: DeserializeOwned>(socket: &Path, req: &Request) -> Result<T> {
    let mut stream = connect(socket)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("failed to set socket timeout")?;
    send(&mut stream, req)?;
    read_line(&mut BufReader::new(stream))
}

/// Streams a `Status` per daemon change, starting with the current one.
pub fn subscribe(socket: &Path) -> Result<impl Iterator<Item = Result<Status>>> {
    let mut stream = connect(socket)?;
    send(&mut stream, &Request::Subscribe)?;
    let mut reader = BufReader::new(stream);
    Ok(std::iter::from_fn(move || Some(read_line(&mut reader))))
}
