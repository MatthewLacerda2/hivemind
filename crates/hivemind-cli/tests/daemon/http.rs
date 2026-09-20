//! One HTTP request to `127.0.0.1`, written by hand over a `TcpStream`.
//!
//! **Why not an HTTP client.** The repo's rule is that HTTP clients here are
//! written against `hyper` directly, and a test harness talking to the
//! loopback API is the easiest case that rule has: one request, one response,
//! no TLS, no redirects, no connection pool. `reqwest` was in the CLI's
//! dev-dependencies for the `blocking` feature alone, and `reqwest::blocking`
//! builds a runtime of its own that panics when it is dropped inside an async
//! context — which is why the harness used to say its three helpers could not
//! be called from a `#[tokio::test]`, and why two earlier copies of it shelled
//! out to `curl` instead (#74, #77).
//!
//! **Framing.** The request says `Connection: close`, so the server hangs up
//! when it has finished and the whole response can be read to the end of the
//! socket. The body is then whatever follows the blank line, trimmed to
//! `Content-Length` where the response gives one — the attachment endpoint
//! does, over a streamed file. Nothing here decodes `Transfer-Encoding:
//! chunked`: [`Response::body`] would be the chunk framing rather than the
//! bytes, so a chunked answer is refused by name rather than returned
//! corrupted. No endpoint on the local API sends one today.
//!
//! Bytes throughout. `get_bytes` is how attachments are fetched, and a body
//! decoded as UTF-8 and re-encoded would turn a blob into replacement
//! characters.

use std::io::{Read as _, Write as _};
use std::time::Duration;

/// What the daemon answered: the status code and the body as it arrived.
#[derive(Debug)]
pub(crate) struct Response {
    /// The three digits of the status line.
    pub(crate) status: u16,
    /// The body, exactly as many bytes as came back.
    pub(crate) body: Vec<u8>,
}

/// Send one request to the loopback API and read the whole answer.
///
/// `json` is the request body, already encoded; `None` sends none. The
/// timeout covers each read and write rather than the exchange, which is
/// enough for a caller that only wants a slow daemon to fail rather than hang
/// — a first attachment fetch blocks while the blob comes from another
/// machine, so `get_bytes` asks for a generous one.
pub(crate) fn request(
    port: u16,
    method: &str,
    path: &str,
    json: Option<&[u8]>,
    timeout: Duration,
) -> Result<Response, String> {
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("could not connect to 127.0.0.1:{port}: {e}"))?;
    let _ = socket.set_read_timeout(Some(timeout));
    let _ = socket.set_write_timeout(Some(timeout));

    let framing = match json {
        Some(body) => format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ),
        None => String::new(),
    };
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{framing}\r\n"
    );

    let mut wire = head.into_bytes();
    wire.extend_from_slice(json.unwrap_or_default());
    socket
        .write_all(&wire)
        .map_err(|e| format!("could not send {method} {path}: {e}"))?;

    let mut answer = Vec::new();
    socket
        .read_to_end(&mut answer)
        .map_err(|e| format!("could not read the answer to {method} {path}: {e}"))?;
    parse(&answer)
}

/// Split a response into its status and its body.
fn parse(answer: &[u8]) -> Result<Response, String> {
    let split = answer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("no header block in {:?}", String::from_utf8_lossy(answer)))?;
    // Headers are ASCII by the standard and by what this daemon sends; the
    // body is not, so only this half is ever treated as text.
    let head = String::from_utf8_lossy(&answer[..split]);
    let body = &answer[split + 4..];

    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("no status line in {head:?}"))?;

    // Every other header is ignored on purpose. The daemon stamps
    // `x-hivemind-binary-modified` on every local response (#36) and may grow
    // more; a client that only reads what it needs does not care.
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            return Err(format!(
                "{head:?} is chunked, and this client does not decode chunks"
            ));
        }
    }

    // `Connection: close` means the read ran to the end of the socket, so a
    // shorter `Content-Length` is the authority and a longer one is a body
    // that was cut off — which is worth saying rather than returning short.
    let body = match length {
        Some(n) if n > body.len() => {
            return Err(format!(
                "the body stopped at {} bytes of a declared {n}",
                body.len()
            ));
        }
        Some(n) => &body[..n],
        None => body,
    };

    Ok(Response {
        status,
        body: body.to_vec(),
    })
}
