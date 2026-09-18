//! A thin client for the local API.
//!
//! The CLI is an HTTP client like any other: it never reads `mail/` directly
//! (SPEC §10). That keeps one implementation of every operation, and it means a
//! CLI bug cannot corrupt the store.

use anyhow::{Context as _, Result, bail};
use serde::de::DeserializeOwned;

/// Talks to `127.0.0.1:8401`.
pub(crate) struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    /// Point a client at a base URL.
    pub(crate) fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }

    /// GET and decode.
    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .map_err(|e| not_running(&self.base, e))?;
        decode(response).await
    }

    /// POST JSON and decode.
    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .map_err(|e| not_running(&self.base, e))?;
        decode(response).await
    }

    /// POST with no response body.
    pub(crate) async fn post_empty(&self, path: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| not_running(&self.base, e))?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(problem_error(response).await)
    }
}

impl Client {
    /// DELETE, discarding the (empty) response body.
    pub(crate) async fn delete(&self, path: &str) -> Result<()> {
        let response = self
            .http
            .delete(format!("{}{path}", self.base))
            .send()
            .await
            .map_err(|e| not_running(&self.base, e))?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(problem_error(response).await)
    }
}

/// The overwhelmingly likely cause of a connection error is that the daemon is
/// not running, so say that instead of showing a transport error.
fn not_running(base: &str, error: reqwest::Error) -> anyhow::Error {
    if error.is_connect() {
        return anyhow::anyhow!(
            "no hivemind daemon at {base}\n\nStart one with `hivemind daemon`, \
             or point elsewhere with --api."
        );
    }
    anyhow::Error::new(error).context(format!("request to {base} failed"))
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    if !response.status().is_success() {
        return Err(problem_error(response).await);
    }
    response
        .json()
        .await
        .context("the daemon sent a response this version does not understand")
}

/// Turn an RFC 9457 problem document back into a sentence (SPEC §7.3).
async fn problem_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let Ok(problem) = response.json::<serde_json::Value>().await else {
        return anyhow::anyhow!("the daemon returned {status}");
    };

    let detail = problem
        .get("detail")
        .and_then(|d| d.as_str())
        .or_else(|| problem.get("title").and_then(|t| t.as_str()))
        .unwrap_or("the daemon rejected the request");
    anyhow::anyhow!("{detail}")
}

/// Read a body argument, falling back to stdin for `-` or nothing at all.
pub(crate) fn body_from_arg_or_stdin(arg: Option<&str>) -> Result<String> {
    match arg {
        Some("-") | None => {
            use std::io::Read as _;
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("could not read the message body from stdin")?;
            if buffer.trim().is_empty() {
                bail!("the message body is empty");
            }
            Ok(buffer)
        }
        Some(text) => Ok(text.to_owned()),
    }
}
