//! Does SIGTERM stop the daemon? Measured, with a client attached and without
//! (#108).
//!
//! Its own file because none of this is about what the daemon *serves*: it is
//! about whether the process goes when `hivemind service restart` and launchd
//! ask it to, and launchd follows a SIGTERM that is ignored with a SIGKILL.
//!
//! **Both directions, or neither proves anything.** A daemon with nothing
//! attached stopping on SIGTERM is the control: without it, a green run here
//! could mean the signal handler works, or could mean this machine was going
//! to end the process whatever happened. The pair is what says the difference
//! is the stream.
//!
//! **One endpoint that never drains is a class, not an instance.** There are
//! two long-lived streams on the loopback listener — `/api/v1/events` and the
//! MCP session at `/mcp` — and both held the process open. A test for each,
//! because they end by different mechanisms and either could come back alone.
//!
//! **Every wait is bounded and every failure has a sentence on it.** A test
//! that joined a process which was never going to exit would hang rather than
//! fail, and a hanging test says nothing about which it was (CLAUDE.md).
//! [`Daemon::terminate_within`] gives up and returns, the sockets here read
//! against a timeout, and `Drop for Daemon` kills whatever is left either way.

mod daemon;

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use daemon::Daemon;

/// How long SIGTERM is given before the daemon is called stuck.
///
/// Generous on purpose, because the failure it guards against is unbounded: a
/// `serve` waiting on a connection that never drains waits for ever, so any
/// finite number catches it, and the only thing a tight bound could add is a
/// red test on a loaded CI runner. Measured on this machine, every case here
/// lands well under a second.
const PATIENCE: Duration = Duration::from_secs(15);

/// Long enough that a busy machine is not what fails, short enough that a
/// daemon answering with nothing is a failure rather than a hang.
const ANSWER: Duration = Duration::from_secs(10);

/// A stream held open against a running daemon.
///
/// Sockets rather than a client library, because what has to be true here is
/// only that a connection is open and the daemon has taken it — and the
/// response head does not arrive until it has.
struct Attached {
    socket: std::net::TcpStream,
}

impl Attached {
    /// Subscribe to `/api/v1/events`, which is what the web UI holds open.
    fn events(daemon: &Daemon) -> Self {
        let mut socket = connect(daemon);
        // No `Connection: close`: this one is meant to stay open, which is the
        // whole point of it.
        write!(
            socket,
            "GET /api/v1/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Accept: text/event-stream\r\n\r\n",
            daemon.port
        )
        .expect("the request goes out");

        Self {
            socket: stream(socket, "/api/v1/events"),
        }
    }

    /// Open an MCP session and its standalone stream, as Claude Code does.
    ///
    /// Two requests, because streamable HTTP has no stream until there is a
    /// session: `initialize` over POST names one in `mcp-session-id`, and the
    /// GET carrying server-to-client messages quotes it back. That GET is what
    /// an editor connected to this daemon holds open all day.
    fn mcp(daemon: &Daemon) -> Self {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"shutdown","version":"1"}}}"#;
        let mut socket = connect(daemon);
        write!(
            socket,
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\
             Accept: application/json, text/event-stream\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            daemon.port,
            body.len()
        )
        .expect("the request goes out");
        socket.write_all(body).expect("the body goes out");

        let head = read_head(&mut socket);
        let session = header(&head, "mcp-session-id").unwrap_or_else(|| {
            panic!("the daemon should have opened a session, and said {head:?}")
        });
        drop(socket);

        let mut socket = connect(daemon);
        write!(
            socket,
            "GET /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Accept: text/event-stream\r\nmcp-session-id: {session}\r\n\r\n",
            daemon.port
        )
        .expect("the request goes out");

        Self {
            socket: stream(socket, "/mcp"),
        }
    }

    /// Everything still to come on the socket, up to the end of it.
    ///
    /// Whatever arrives, errors included: a connection reset is an answer to
    /// the question being asked here, not a reason to panic before asking it.
    fn drain(mut self) -> String {
        let mut said = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(read) = self.socket.read(&mut chunk) {
            if read == 0 {
                break;
            }
            said.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&said).into_owned()
    }
}

/// A socket to this daemon's loopback API, with reads bounded.
fn connect(daemon: &Daemon) -> std::net::TcpStream {
    let socket = std::net::TcpStream::connect(("127.0.0.1", daemon.port))
        .expect("the daemon accepts a connection");
    socket
        .set_read_timeout(Some(ANSWER))
        .expect("a read timeout");
    socket
}

/// Read the response head and check it really is an open stream.
fn stream(mut socket: std::net::TcpStream, what: &str) -> std::net::TcpStream {
    let head = read_head(&mut socket);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{what} should have answered with a stream, and said {head:?}"
    );
    assert!(
        head.to_ascii_lowercase().contains("text/event-stream"),
        "and it should be an event stream: {head:?}"
    );
    socket
}

/// Read up to the blank line that ends the response head.
///
/// A byte at a time, because anything more would swallow the start of the body
/// — and against the socket's read timeout, so a daemon that answers with
/// nothing fails rather than hangs.
fn read_head(socket: &mut std::net::TcpStream) -> String {
    let deadline = Instant::now() + ANSWER;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while Instant::now() < deadline {
        match socket.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => head.push(byte[0]),
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// One header's value out of a response head, whatever case it was sent in.
fn header(head: &str, name: &str) -> Option<String> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(field, _)| field.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_owned())
}

#[test]
fn a_daemon_with_nothing_attached_stops_on_sigterm() {
    let mut daemon = Daemon::start("quiet");

    let took = daemon.terminate_within(PATIENCE);

    assert!(
        took.is_some(),
        "a daemon with no clients at all ignored SIGTERM for {PATIENCE:?}; \
         nothing below this proves anything until that works"
    );
}

#[test]
fn a_daemon_with_an_event_stream_attached_stops_on_sigterm() {
    let mut daemon = Daemon::start("streamed");
    let attached = Attached::events(&daemon);

    let took = daemon.terminate_within(PATIENCE);

    assert!(
        took.is_some(),
        "a daemon with an event stream attached ignored SIGTERM for {PATIENCE:?}: \
         `/api/v1/events` has to end when the daemon is shutting down, or the \
         graceful shutdown waits on a connection that never drains and launchd \
         finishes the job with SIGKILL (#108)"
    );

    let tail = attached.drain();
    assert!(
        tail.ends_with("0\r\n\r\n"),
        "and the stream should end cleanly, chunked terminator and all, so a \
         client can tell a shutdown from a crash; it ended with {tail:?}"
    );
}

#[test]
fn a_daemon_with_an_mcp_session_attached_stops_on_sigterm() {
    // The same defect one endpoint along, and the likelier one to be hit: a
    // Claude Code with this daemon configured holds the session stream open
    // for as long as the editor is running. `rmcp` has no shutdown of its
    // own, so the sessions are closed by hand (#108).
    let mut daemon = Daemon::start("mcp-streamed");
    let _attached = Attached::mcp(&daemon);

    let took = daemon.terminate_within(PATIENCE);

    assert!(
        took.is_some(),
        "a daemon with an MCP session attached ignored SIGTERM for {PATIENCE:?}: \
         the session's stream has to be closed on the way out, or it holds the \
         connection in flight and the graceful shutdown waits on it (#108)"
    );
}
