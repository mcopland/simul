//! Claude Code's HTTP hooks: tool events POSTed to the daemon, so a tool call starts no
//! `simul hook` process. Only 127.0.0.1 is served, and only `POST /simul/hook/<agent>/<event>`.

use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::Cmd;
use crate::hook::slim_payload;
use crate::log;
use crate::paths;
use crate::protocol::{Envelope, Request};

/// Where hooks post: the agent and event follow, as in `simul hook <agent> <event>`.
pub const PATH_PREFIX: &str = "/simul/hook/";
/// Claude fills these from the pane's environment (see `allowedEnvVars` in install-hooks).
pub const PANE_HEADER: &str = "X-Simul-Pane";
pub const TMUX_HEADER: &str = "X-Simul-Tmux";

/// Request line and headers together; hooks send a few short headers.
const MAX_HEAD: usize = 64 * 1024;
/// As for `simul hook`'s stdin: tool results can be large, and are slimmed after reading.
const MAX_BODY: usize = 16 * 1024 * 1024;
/// How often a daemon that could not take the port tries again. Another daemon holds it,
/// often one being replaced, or one for another tmux server.
const BIND_RETRY: Duration = Duration::from_secs(5);
/// The pause after `accept` fails for lack of resources, before the next try.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);
/// How long a kept-alive connection may wait for its next request; Claude opens a new one
/// when it needs to, and an idle one would hold a task and a descriptor for good.
const IDLE: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The client asked to close after this request, or speaks HTTP/1.0.
    pub close: bool,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A request the reader refused; the connection closes after the reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    BadRequest,
    TooLarge,
    /// Chunked bodies are not supported; hooks send a `Content-Length`.
    LengthRequired,
}

impl Reject {
    fn status(self) -> u16 {
        match self {
            Reject::BadRequest => 400,
            Reject::TooLarge => 413,
            Reject::LengthRequired => 411,
        }
    }
}

/// Reads one request. `Ok(None)` is a clean end of the connection between requests.
pub async fn read_request<R: AsyncBufRead + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<Result<HttpRequest, Reject>>> {
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let limit = u64::try_from(MAX_HEAD - total).unwrap_or(0) + 1;
        let n = (&mut *r).take(limit).read_until(b'\n', &mut line).await?;
        if n == 0 {
            return Ok((!lines.is_empty()).then_some(Err(Reject::BadRequest)));
        }
        total += n;
        if total > MAX_HEAD {
            return Ok(Some(Err(Reject::TooLarge)));
        }
        if !line.ends_with(b"\n") {
            // The stream ended inside a line.
            return Ok(Some(Err(Reject::BadRequest)));
        }
        let Ok(text) = String::from_utf8(line) else {
            return Ok(Some(Err(Reject::BadRequest)));
        };
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            // Blank lines before a request line are allowed (RFC 9112, 2.2).
            if lines.is_empty() {
                continue;
            }
            break;
        }
        lines.push(text.to_owned());
    }
    let mut parts = lines[0].split_whitespace();
    let (Some(method), Some(path), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Ok(Some(Err(Reject::BadRequest)));
    };
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        _ => return Ok(Some(Err(Reject::BadRequest))),
    };
    let mut headers = Vec::new();
    for line in &lines[1..] {
        let Some((name, value)) = line.split_once(':') else {
            return Ok(Some(Err(Reject::BadRequest)));
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let get = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    if get("transfer-encoding").is_some() {
        return Ok(Some(Err(Reject::LengthRequired)));
    }
    let len = match get("content-length").map(str::parse::<usize>) {
        None => 0,
        Some(Ok(n)) if n > MAX_BODY => return Ok(Some(Err(Reject::TooLarge))),
        Some(Ok(n)) => n,
        Some(Err(_)) => return Ok(Some(Err(Reject::BadRequest))),
    };
    let close = match get("connection") {
        Some(c) if c.eq_ignore_ascii_case("close") => true,
        Some(c) if c.eq_ignore_ascii_case("keep-alive") => false,
        _ => http10,
    };
    let mut body = vec![0; len];
    match r.read_exact(&mut body).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(Some(Err(Reject::BadRequest))),
        Err(e) => return Err(e),
    }
    Ok(Some(Ok(HttpRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body,
        close,
    })))
}

/// What a request asks of the daemon.
#[derive(Debug, PartialEq)]
pub enum Hook {
    /// A hook to apply or forward.
    Envelope(Envelope),
    /// A hook from outside tmux: nothing to track, as with `simul hook`.
    NoPane,
    /// Answered with this status and dropped.
    Refused(u16),
}

/// Turns a request into the envelope `simul hook` would have sent.
pub fn hook_of(req: &HttpRequest) -> Hook {
    if req.method != "POST" {
        return Hook::Refused(405);
    }
    let Some((agent, event)) = req
        .path
        .strip_prefix(PATH_PREFIX)
        .and_then(|rest| rest.split_once('/'))
        .filter(|(a, e)| !a.is_empty() && !e.is_empty() && !e.contains('/'))
    else {
        return Hook::Refused(404);
    };
    let present = |name| {
        req.header(name)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let Some(pane) = present(PANE_HEADER) else {
        return Hook::NoPane;
    };
    // A malformed payload still carries the event name, which is enough to update state.
    let payload = match serde_json::from_slice::<Value>(&req.body) {
        Ok(v) => slim_payload(&v),
        Err(e) => {
            log!(
                "HTTP hook {agent} {event}: payload is not JSON ({e}); applying the event without it"
            );
            Value::Null
        }
    };
    Hook::Envelope(Envelope {
        agent: agent.to_owned(),
        event: event.to_owned(),
        pane: Some(pane),
        tmux: present(TMUX_HEADER),
        payload,
    })
}

/// Which daemon a hook belongs to: the one whose socket `simul hook` would have found.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Local,
    /// Another tmux server's daemon, at the first of these sockets that answers.
    Forward(Vec<PathBuf>),
    /// No tmux server is known, so no daemon can place the pane.
    Drop,
}

pub fn route(
    tmux: Option<&str>,
    own: &Path,
    env: impl Fn(&str) -> Option<String>,
    uid: u32,
) -> Route {
    let socket = tmux.and_then(paths::tmux_socket_from_env);
    match paths::socket_candidates(env, uid, socket.as_deref()) {
        Ok(sockets) if sockets.iter().any(|s| s == own) => Route::Local,
        Ok(sockets) => Route::Forward(sockets),
        Err(_) => Route::Drop,
    }
}

/// Serves `127.0.0.1:port` for as long as the daemon runs. While another daemon holds the
/// port, it tries again every `BIND_RETRY`, logging each new failure once.
pub async fn serve_port(port: u16, tx: mpsc::Sender<Cmd>, own: PathBuf) {
    let mut last_error: Option<String> = None;
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => {
                log!("serving HTTP hooks on 127.0.0.1:{port}");
                accept(listener, tx, own).await;
                return;
            }
            Err(e) => {
                let error = e.to_string();
                if last_error.as_deref() != Some(&error) {
                    log!(
                        "failed to listen for HTTP hooks on 127.0.0.1:{port}: {error}; retrying every {BIND_RETRY:?}. If another program uses the port, set [hooks] http_port and rerun `simul install-hooks claude`"
                    );
                    last_error = Some(error);
                }
                tokio::time::sleep(BIND_RETRY).await;
            }
        }
    }
}

async fn accept(listener: TcpListener, tx: mpsc::Sender<Cmd>, own: PathBuf) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let tx = tx.clone();
                let own = own.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(stream, &tx, &own, IDLE).await
                        && !super::server::is_disconnect(&e)
                    {
                        log!("HTTP hook connection: {e:#}");
                    }
                });
            }
            Err(e) => {
                log!("HTTP hook accept failed: {e}");
                // As in `server::accept`: resource exhaustion lasts, other errors don't.
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
                ) {
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            }
        }
    }
}

async fn serve(
    stream: TcpStream,
    tx: &mpsc::Sender<Cmd>,
    own: &Path,
    idle: Duration,
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    loop {
        let Ok(read) = tokio::time::timeout(idle, read_request(&mut r)).await else {
            return Ok(());
        };
        let req = match read.context("failed to read HTTP hook request")? {
            None => return Ok(()),
            Some(Ok(req)) => req,
            Some(Err(reject)) => return respond(&mut w, reject.status(), true).await,
        };
        let status = match hook_of(&req) {
            Hook::Envelope(env) => deliver(env, tx, own).await,
            Hook::NoPane => 200,
            Hook::Refused(status) => status,
        };
        respond(&mut w, status, req.close).await?;
        if req.close {
            return Ok(());
        }
    }
}

/// Applies or forwards a hook. Claude only notes a failure and goes on, so a hook that can't be
/// delivered is logged and still answered 200, except while this daemon shuts down.
async fn deliver(env: Envelope, tx: &mpsc::Sender<Cmd>, own: &Path) -> u16 {
    match route(env.tmux.as_deref(), own, paths::env_var, paths::uid()) {
        Route::Local => match tx.send(Cmd::Hook(env)).await {
            Ok(()) => 200,
            Err(_) => 503,
        },
        Route::Forward(sockets) => {
            let tmux = env.tmux.clone().unwrap_or_default();
            match tokio::task::spawn_blocking(move || forward(&env, &sockets)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log!("failed to forward HTTP hook for tmux {tmux}: {e:#}"),
                Err(e) => log!("HTTP hook forwarder for tmux {tmux} panicked: {e}"),
            }
            200
        }
        Route::Drop => 200,
    }
}

/// Sends a hook to another tmux server's daemon, as `simul hook` would, without starting it:
/// its next command hook does that.
fn forward(env: &Envelope, sockets: &[PathBuf]) -> Result<()> {
    let mut line = serde_json::to_string(&Request::Hook(env.clone()))
        .context("failed to encode hook envelope")?;
    line.push('\n');
    let Some(mut stream) = crate::hook::shim::connect_any(sockets) else {
        anyhow::bail!(
            "no simul daemon answers at {}",
            sockets
                .iter()
                .map(|s| s.display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        );
    };
    std::io::Write::write_all(&mut stream, line.as_bytes())
        .context("failed to write hook to the simul daemon")
}

async fn respond<W: AsyncWrite + Unpin>(w: &mut W, status: u16, close: bool) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Content Too Large",
        _ => "Service Unavailable",
    };
    let connection = if close { "Connection: close\r\n" } else { "" };
    let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n{connection}\r\n");
    w.write_all(head.as_bytes())
        .await
        .context("failed to write HTTP hook reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn post(path: &str, headers: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    async fn read_all(bytes: &[u8]) -> Vec<Result<HttpRequest, Reject>> {
        let mut r = bytes;
        let mut out = Vec::new();
        while let Some(req) = read_request(&mut r).await.unwrap() {
            let done = req.as_ref().map_or(true, |r| r.close);
            out.push(req);
            if done {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn reads_a_post_with_headers_and_body() {
        let raw = post(
            "/simul/hook/claude/PreToolUse",
            "x-simul-PANE: %3\r\n",
            "{}",
        );
        let reqs = read_all(raw.as_bytes()).await;
        let req = reqs[0].as_ref().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/simul/hook/claude/PreToolUse");
        assert_eq!(req.header(PANE_HEADER), Some("%3"), "names match any case");
        assert_eq!(req.body, b"{}");
        assert!(!req.close, "HTTP/1.1 keeps the connection");
    }

    #[tokio::test]
    async fn reads_requests_kept_alive_on_one_connection() {
        let raw = format!(
            "{}{}",
            post("/a", "", "one"),
            post("/b", "Connection: close\r\n", "two")
        );
        let reqs = read_all(raw.as_bytes()).await;
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].as_ref().unwrap().body, b"one");
        let last = reqs[1].as_ref().unwrap();
        assert_eq!((last.path.as_str(), last.close), ("/b", true));
    }

    #[tokio::test]
    async fn reads_a_request_split_across_writes() {
        let raw = post(
            "/simul/hook/claude/PostToolUse",
            "",
            r#"{"tool_name":"Edit"}"#,
        );
        let (mut client, server) = tokio::io::duplex(8);
        let writer = tokio::spawn(async move {
            for b in raw.as_bytes() {
                client.write_all(&[*b]).await.unwrap();
            }
        });
        let mut r = BufReader::new(server);
        let req = read_request(&mut r).await.unwrap().unwrap().unwrap();
        writer.await.unwrap();
        assert_eq!(req.body, br#"{"tool_name":"Edit"}"#);
    }

    #[tokio::test]
    async fn an_end_between_requests_is_clean_and_inside_one_is_not() {
        assert_eq!(read_request(&mut &b""[..]).await.unwrap(), None);
        let cut = b"POST /a HTTP/1.1\r\nHost: x\r\n";
        assert_eq!(
            read_request(&mut &cut[..]).await.unwrap(),
            Some(Err(Reject::BadRequest))
        );
        let short = b"POST /a HTTP/1.1\r\nContent-Length: 5\r\n\r\nab";
        assert_eq!(
            read_request(&mut &short[..]).await.unwrap(),
            Some(Err(Reject::BadRequest))
        );
    }

    #[tokio::test]
    async fn refuses_oversized_and_chunked_requests() {
        let big = format!(
            "POST /a HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(
            read_request(&mut big.as_bytes()).await.unwrap(),
            Some(Err(Reject::TooLarge)),
            "refused before the body is read"
        );
        let head = format!("POST /a HTTP/1.1\r\nX: {}\r\n\r\n", "y".repeat(MAX_HEAD));
        assert_eq!(
            read_request(&mut head.as_bytes()).await.unwrap(),
            Some(Err(Reject::TooLarge))
        );
        let chunked = b"POST /a HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert_eq!(
            read_request(&mut &chunked[..]).await.unwrap(),
            Some(Err(Reject::LengthRequired))
        );
    }

    #[tokio::test]
    async fn refuses_malformed_heads() {
        for raw in [
            "POST /a\r\n\r\n",
            "POST /a HTTP/2\r\n\r\n",
            "POST /a HTTP/1.1\r\nno colon\r\n\r\n",
            "POST /a HTTP/1.1\r\nContent-Length: x\r\n\r\n",
        ] {
            assert_eq!(
                read_request(&mut raw.as_bytes()).await.unwrap(),
                Some(Err(Reject::BadRequest)),
                "{raw:?}"
            );
        }
    }

    #[tokio::test]
    async fn http_1_0_closes_unless_kept_alive() {
        let raw = b"POST /a HTTP/1.0\r\n\r\n";
        let req = read_request(&mut &raw[..]).await.unwrap().unwrap().unwrap();
        assert!(req.close);
        let raw = b"POST /a HTTP/1.0\r\nConnection: keep-alive\r\n\r\n";
        let req = read_request(&mut &raw[..]).await.unwrap().unwrap().unwrap();
        assert!(!req.close);
    }

    fn request(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            path: path.into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_ascii_lowercase(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
            close: false,
        }
    }

    #[test]
    fn a_hook_post_becomes_the_envelope_simul_hook_sends() {
        let big = "x".repeat(5000);
        let body = json!({"tool_name": "Edit", "tool_response": big}).to_string();
        let req = request(
            "POST",
            "/simul/hook/claude/PostToolUse",
            &[
                (PANE_HEADER, "%7"),
                (TMUX_HEADER, "/tmp/tmux-501/default,1,0"),
            ],
            &body,
        );
        assert_eq!(
            hook_of(&req),
            Hook::Envelope(Envelope {
                agent: "claude".into(),
                event: "PostToolUse".into(),
                pane: Some("%7".into()),
                tmux: Some("/tmp/tmux-501/default,1,0".into()),
                payload: json!({"tool_name": "Edit"}),
            }),
            "large fields are slimmed away"
        );
    }

    #[test]
    fn a_hook_without_a_pane_or_with_bad_json_degrades_like_simul_hook() {
        let req = request(
            "POST",
            "/simul/hook/claude/PreToolUse",
            &[(PANE_HEADER, "")],
            "{}",
        );
        assert_eq!(hook_of(&req), Hook::NoPane, "empty: $TMUX_PANE was unset");
        let req = request(
            "POST",
            "/simul/hook/claude/Stop",
            &[(PANE_HEADER, "%1")],
            "nope",
        );
        let Hook::Envelope(env) = hook_of(&req) else {
            panic!("bad JSON still carries the event");
        };
        assert_eq!((env.event.as_str(), env.payload), ("Stop", Value::Null));
        assert_eq!(env.tmux, None);
    }

    #[test]
    fn only_posts_to_hook_paths_are_hooks() {
        let pane = [(PANE_HEADER, "%1")];
        assert_eq!(
            hook_of(&request("GET", "/simul/hook/claude/Stop", &pane, "")),
            Hook::Refused(405)
        );
        for path in [
            "/",
            "/simul/hook/claude",
            "/simul/hook/claude/",
            "/simul/hook//Stop",
            "/simul/hook/claude/Stop/x",
            "/other/claude/Stop",
        ] {
            assert_eq!(
                hook_of(&request("POST", path, &pane, "")),
                Hook::Refused(404),
                "{path}"
            );
        }
    }

    #[test]
    fn routes_to_the_daemon_simul_hook_would_find() {
        let env = |_: &str| None;
        let own = paths::socket_candidates(env, 501, Some(Path::new("/tmp/tmux-501/default")))
            .unwrap()
            .remove(0);
        assert_eq!(
            route(Some("/tmp/tmux-501/default,9,0"), &own, env, 501),
            Route::Local
        );
        let other =
            paths::socket_candidates(env, 501, Some(Path::new("/tmp/tmux-501/work"))).unwrap();
        assert_eq!(
            route(Some("/tmp/tmux-501/work,9,0"), &own, env, 501),
            Route::Forward(other)
        );
        assert_eq!(route(None, &own, env, 501), Route::Drop);
        assert_eq!(route(Some(""), &own, env, 501), Route::Drop);
    }

    #[test]
    fn an_explicit_socket_routes_every_hook_to_it() {
        let own = PathBuf::from("/x/s.sock");
        let env = |k: &str| (k == "SIMUL_SOCKET").then(|| "/x/s.sock".to_owned());
        assert_eq!(route(None, &own, env, 501), Route::Local);
        assert_eq!(route(Some("/tmp/t,1,0"), &own, env, 501), Route::Local);
    }

    #[tokio::test]
    async fn an_idle_connection_is_closed() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, _rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve(
                stream,
                &tx,
                Path::new("/x/s.sock"),
                Duration::from_millis(50),
            )
            .await
        });
        let mut conn = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf)).await;
        assert!(
            matches!(read, Ok(Ok(0))),
            "expected the daemon to close: {read:?}"
        );
    }

    #[tokio::test]
    async fn serves_hooks_over_tcp_and_keeps_the_connection() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let tmux = "/tmp/simul-http-test-tmux";
        // `deliver` routes with the real environment, so this daemon takes the socket that
        // environment gives the tmux server the hook names.
        let own = paths::socket_candidates(paths::env_var, paths::uid(), Some(Path::new(tmux)))
            .unwrap()
            .remove(0);
        tokio::spawn(accept(listener, tx, own));
        let mut conn = TcpStream::connect(addr).await.unwrap();
        let headers = format!("{PANE_HEADER}: %4\r\n{TMUX_HEADER}: {tmux},1,0\r\n");
        let one = post(
            "/simul/hook/claude/PreToolUse",
            &headers,
            r#"{"tool_name":"Bash"}"#,
        );
        let two = post("/nope", "", "");
        conn.write_all(format!("{one}{two}").as_bytes())
            .await
            .unwrap();
        let mut replies = String::new();
        let mut buf = [0u8; 256];
        while replies.matches("\r\n\r\n").count() < 2 {
            let n = conn.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed early: {replies:?}");
            replies.push_str(std::str::from_utf8(&buf[..n]).unwrap());
        }
        assert!(replies.starts_with("HTTP/1.1 200 OK\r\n"), "{replies}");
        assert!(replies.contains("HTTP/1.1 404 Not Found\r\n"), "{replies}");
        let Some(Cmd::Hook(env)) = rx.recv().await else {
            panic!("expected a hook");
        };
        assert_eq!(env.event, "PreToolUse");
        assert_eq!(env.pane.as_deref(), Some("%4"));
        assert_eq!(env.payload, json!({"tool_name": "Bash"}));
    }
}
