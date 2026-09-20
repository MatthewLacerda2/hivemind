//! The daemon's event stream (SPEC §7.1), as events to await one at a time.
//!
//! `hivemind wait` is the only caller: it needs to know the moment mail
//! arrives, and the alternative is the polling loop the issue that asked for
//! it had already been written four times by hand (#40). The web UI consumes
//! the same stream through `EventSource`, and for the same reason it does not
//! render from it — the event says *that* something happened and which id it
//! was about, and the listing endpoint is what says what it looks like.
//!
//! Hand-rolled on hyper, like `client.rs`, and for the measured reason given
//! there: `reqwest` brings a second cryptography backend into a workspace that
//! chose one, for requests to `127.0.0.1` that use no TLS at all.
//!
//! The framing is split from the socket on purpose. Deciding where one event
//! ends is where an SSE reader goes wrong — a keep-alive comment is not an
//! event, and a chunk boundary can land anywhere — and both of those are
//! judgements that need no network to test.

use anyhow::{Context as _, Result, bail};
use http_body_util::BodyExt as _;

/// One event off the stream: its name, and the id it is about (SPEC §7.1).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Event {
    /// `message.received`, `peer.online`, and the rest.
    pub(crate) name: String,
    /// The message or node id the event is about.
    pub(crate) data: String,
}

/// Take the first whole event out of `buffer`, leaving the rest.
///
/// `None` means there is no complete event in there yet, so the caller reads
/// more. Blocks that are not events are consumed rather than returned: a
/// keep-alive between two events must not stop the second one being found.
fn take(buffer: &mut String) -> Option<Event> {
    while let Some(end) = buffer.find("\n\n") {
        let block: String = buffer.drain(..end + 2).collect();
        if let Some(event) = parse(&block) {
            return Some(event);
        }
    }
    None
}

/// One blank-line-terminated block as an event.
///
/// `None` for a block with no `event:` field, which is how axum's keep-alive
/// arrives: a bare `:` comment every fifteen seconds, there to hold the
/// connection open through anything that times an idle socket out. Treating
/// one as an event would wake a waiter up for nothing.
fn parse(block: &str) -> Option<Event> {
    let mut name: Option<String> = None;
    let mut data = String::new();

    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("data:") {
            // SSE allows several data lines in one event, joined by newlines.
            // Nothing this daemon sends uses that, and a reader that dropped
            // all but the last would be wrong the day something does.
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim());
        }
    }

    Some(Event { name: name?, data })
}

/// An open subscription to `/api/v1/events`.
pub(crate) struct Stream {
    // Kept for the error a waiter sees when the stream stops: "the daemon
    // stopped sending events" with nothing naming which daemon is a sentence
    // that has to be guessed at.
    base: String,
    body: hyper::body::Incoming,
    buffer: String,
    // Held, not dropped: in hyper 1.x the connection finishes once the last
    // `SendRequest` goes away, and this response body never ends by design.
    _sender: hyper::client::conn::http1::SendRequest<http_body_util::Empty<hyper::body::Bytes>>,
    // Drives the socket. Nothing awaits it: it ends when the daemon does.
    pump: tokio::task::JoinHandle<()>,
}

impl Stream {
    /// Subscribe to the daemon's events.
    ///
    /// # Errors
    /// When there is nothing listening, or it answers with something other
    /// than a stream.
    pub(crate) async fn open(base: &str) -> Result<Self> {
        let authority = crate::client::authority(base);
        let socket = tokio::net::TcpStream::connect(authority)
            .await
            .map_err(|_| crate::client::not_running(base))?;
        let _ = socket.set_nodelay(true);

        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(socket))
                .await
                .with_context(|| format!("could not talk to {base}"))?;
        let pump = tokio::spawn(async move {
            let _ = connection.await;
        });

        let request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri("/api/v1/events")
            .header(hyper::header::HOST, authority)
            .header(hyper::header::ACCEPT, "text/event-stream")
            .body(http_body_util::Empty::new())
            .context("could not build the request")?;

        let response = sender
            .send_request(request)
            .await
            .with_context(|| format!("could not subscribe to events at {base}"))?;

        crate::freshness::note(
            response
                .headers()
                .get(hivemind_api::freshness::BINARY_MODIFIED)
                .and_then(|stamp| stamp.to_str().ok()),
        );

        if !response.status().is_success() {
            pump.abort();
            bail!(
                "{base} refused an event subscription with {}",
                response.status()
            );
        }

        Ok(Self {
            base: base.to_owned(),
            body: response.into_body(),
            buffer: String::new(),
            _sender: sender,
            pump,
        })
    }

    /// The next event, waiting as long as it takes.
    ///
    /// # Errors
    /// When the stream stops, however it stops. There is no `None` here on
    /// purpose: `/api/v1/events` never ends while the daemon is up (SPEC §7.1),
    /// so an end and a broken connection are one fact — it has gone away — and
    /// a caller that cannot tell either of them from "nothing yet" waits for
    /// ever, which is the silence this command exists to end (#40). Kept apart,
    /// the clean-end branch was also unreachable: a daemon that is killed
    /// leaves a truncated chunked body, which arrives as the error below.
    pub(crate) async fn next(&mut self) -> Result<Event> {
        loop {
            if let Some(event) = take(&mut self.buffer) {
                return Ok(event);
            }
            match self.body.frame().await {
                None => bail!("{}", self.stopped()),
                Some(Err(why)) => {
                    return Err(anyhow::Error::new(why).context(self.stopped()));
                }
                Some(Ok(frame)) => {
                    if let Some(chunk) = frame.data_ref() {
                        // Lossy: a partial UTF-8 sequence at a chunk boundary
                        // is possible in principle, and every field the daemon
                        // sends is an id in ASCII.
                        self.buffer.push_str(&String::from_utf8_lossy(chunk));
                    }
                }
            }
        }
    }
}

impl Stream {
    /// What to say when the events stop coming.
    fn stopped(&self) -> String {
        format!(
            "the daemon at {} stopped sending events — check it is still running with `hivemind status`",
            self.base
        )
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_event_is_taken_and_the_rest_is_left() {
        let mut buffer =
            "event: message.received\ndata: 01JXT2\n\nevent: peer.online\ndata: hm1:aa\n\n"
                .to_owned();

        let first = take(&mut buffer).expect("the first event");
        assert_eq!(first.name, "message.received");
        assert_eq!(first.data, "01JXT2");
        assert_eq!(
            take(&mut buffer).expect("the second event").name,
            "peer.online"
        );
        assert_eq!(take(&mut buffer), None, "and then nothing is left");
    }

    #[test]
    fn half_an_event_is_not_an_event_yet() {
        // A chunk boundary can land anywhere, and an event read early would
        // carry an empty id — which is what a waiter would then look up.
        let mut buffer = "event: message.rec".to_owned();
        assert_eq!(take(&mut buffer), None);
        buffer.push_str("eived\ndata: 01JXT2\n\n");
        assert_eq!(
            take(&mut buffer),
            Some(Event {
                name: "message.received".to_owned(),
                data: "01JXT2".to_owned(),
            })
        );
    }

    #[test]
    fn a_keep_alive_comment_is_not_an_event() {
        // Fifteen seconds of silence produces one of these, and a `wait` that
        // took it for mail would print an empty inbox and exit 0.
        let mut buffer = ":\n\nevent: message.received\ndata: 01JXT2\n\n".to_owned();
        assert_eq!(
            take(&mut buffer)
                .expect("the real event past the comment")
                .name,
            "message.received"
        );
    }

    #[test]
    fn a_block_without_an_event_name_is_not_an_event() {
        assert_eq!(parse("data: 01JXT2\n\n"), None);
    }
}
