//! A thin client for the local API.
//!
//! The CLI is an HTTP client like any other: it never reads `mail/` directly
//! (SPEC §10). That keeps one implementation of every operation, and it means a
//! CLI bug cannot corrupt the store.
//!
//! Hand-rolled on hyper rather than `reqwest`, and the reason is measured
//! rather than aesthetic. This talks plain HTTP to `127.0.0.1` and uses no TLS
//! at all, but `reqwest` brings 107 crates — about twenty of which nothing else
//! here needs — including `aws-lc-rs` and its C library `aws-lc-sys`, pulled in
//! through `hyper-rustls`.
//!
//! That is not only weight. Cargo unifies features across the build, so
//! `hyper-rustls` asking `rustls` for `aws-lc-rs` turns it on for
//! `hivemind-net` too, next to the `ring` this workspace actually chose. Two
//! providers compiled in is what made `rustls` unable to pick a default and
//! panic during M3. The fix then was to name `ring` explicitly, which is still
//! right; removing the second provider is what makes it belt and braces rather
//! than load-bearing.

use anyhow::{Context as _, Result, bail};
use http_body_util::BodyExt as _;
use serde::de::DeserializeOwned;

/// How long to wait on the loopback API before giving up.
///
/// Generous, because one call is not a request: `GET /attachments/{sha}` can
/// block while the daemon fetches a gigabyte from another machine.
const TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);

/// What came back.
struct Response {
    status: hyper::StatusCode,
    body: hyper::body::Bytes,
}

/// Talks to `127.0.0.1:8401`.
pub(crate) struct Client {
    base: String,
    timeout: std::time::Duration,
}

impl Client {
    /// Point a client at a base URL.
    pub(crate) fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            timeout: TIMEOUT,
        }
    }

    /// Give up sooner than [`TIMEOUT`].
    ///
    /// For the hooks, which run on every turn boundary and have a hundred
    /// milliseconds to spend (SPEC §9.3). Waiting five minutes on a daemon
    /// that is not there would stop somebody working, which is the one thing
    /// a hook must never do.
    pub(crate) fn impatient(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// GET and decode.
    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.send(hyper::Method::GET, path, None).await?;
        decode(&self.base, &response)
    }

    /// POST a JSON body and decode the answer.
    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let response = self
            .send(
                hyper::Method::POST,
                path,
                Some(body.to_string().into_bytes()),
            )
            .await?;
        decode(&self.base, &response)
    }

    /// POST with no response body worth reading.
    pub(crate) async fn post_empty(&self, path: &str) -> Result<()> {
        let response = self
            .send(hyper::Method::POST, path, Some(b"{}".to_vec()))
            .await?;
        if response.status.is_success() {
            return Ok(());
        }
        Err(problem_error(&response))
    }

    /// POST a JSON body with no response body worth reading.
    pub(crate) async fn post_empty_body(&self, path: &str, body: &serde_json::Value) -> Result<()> {
        let response = self
            .send(
                hyper::Method::POST,
                path,
                Some(body.to_string().into_bytes()),
            )
            .await?;
        if response.status.is_success() {
            return Ok(());
        }
        Err(problem_error(&response))
    }

    /// DELETE, discarding the (empty) response body.
    pub(crate) async fn delete(&self, path: &str) -> Result<()> {
        let response = self.send(hyper::Method::DELETE, path, None).await?;
        if response.status.is_success() {
            return Ok(());
        }
        Err(problem_error(&response))
    }

    /// One request, from connect to collected body.
    async fn send(
        &self,
        method: hyper::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        let authority = authority(&self.base);

        match tokio::time::timeout(self.timeout, self.exchange(authority, method, path, body)).await
        {
            Ok(result) => result,
            Err(_) => bail!(
                "the daemon at {} did not answer within {:?}",
                self.base,
                self.timeout
            ),
        }
    }

    async fn exchange(
        &self,
        authority: &str,
        method: hyper::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        let stream = tokio::net::TcpStream::connect(authority)
            .await
            .map_err(|_| not_running(&self.base))?;
        let _ = stream.set_nodelay(true);

        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .with_context(|| format!("could not talk to {}", self.base))?;

        // Drives the socket; ends when `sender` is dropped.
        let pump = tokio::spawn(async move {
            let _ = connection.await;
        });

        let result = async {
            let mut builder = hyper::Request::builder()
                .method(method)
                .uri(path)
                .header(hyper::header::HOST, authority);
            if body.is_some() {
                builder = builder.header(hyper::header::CONTENT_TYPE, "application/json");
            }

            let request = builder
                .body(http_body_util::Full::new(hyper::body::Bytes::from(
                    body.unwrap_or_default(),
                )))
                .context("could not build the request")?;

            let response = sender
                .send_request(request)
                .await
                .with_context(|| format!("request to {} failed", self.base))?;

            let status = response.status();
            // Every response says which binary answered it, so the question
            // "am I talking to the code that is installed" costs nothing to
            // ask and gets asked on every command rather than on `doctor`
            // alone (#36).
            crate::freshness::note(
                response
                    .headers()
                    .get(hivemind_api::freshness::BINARY_MODIFIED)
                    .and_then(|stamp| stamp.to_str().ok()),
            );
            let collected = response
                .into_body()
                .collect()
                .await
                .with_context(|| format!("could not read the answer from {}", self.base))?;

            Ok(Response {
                status,
                body: collected.to_bytes(),
            })
        }
        .await;

        drop(sender);
        pump.abort();
        result
    }
}

/// The `host:port` to connect to, from the base URL a caller was given.
///
/// Shared with `events.rs`, which opens its own connection because the body it
/// reads is a stream rather than something to collect. Two spellings of the
/// same parsing would be one of them going wrong on `--api` with a trailing
/// slash.
pub(crate) fn authority(base: &str) -> &str {
    base.trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
}

/// The overwhelmingly likely cause of a connection error is that the daemon is
/// not running, so say that instead of showing a transport error.
pub(crate) fn not_running(base: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no hivemind daemon at {base}\n\nStart one with `hivemind daemon`, \
         or point elsewhere with --api."
    )
}

fn decode<T: DeserializeOwned>(base: &str, response: &Response) -> Result<T> {
    if !response.status.is_success() {
        return Err(problem_error(response));
    }
    serde_json::from_slice(&response.body)
        .with_context(|| format!("{base} sent a response this version does not understand"))
}

/// Turn an RFC 9457 problem document back into a sentence (SPEC §7.3).
fn problem_error(response: &Response) -> anyhow::Error {
    let Ok(problem) = serde_json::from_slice::<serde_json::Value>(&response.body) else {
        return anyhow::anyhow!("the daemon returned {}", response.status);
    };

    let detail = problem
        .get("detail")
        .and_then(|d| d.as_str())
        .or_else(|| problem.get("title").and_then(|t| t.as_str()))
        .unwrap_or("the daemon rejected the request");
    anyhow::anyhow!("{detail}")
}
