//! The loopback router (SPEC §7.1).
//!
//! Binds to `127.0.0.1` and fails closed if configured otherwise: it carries no
//! authentication, so reachability *is* the authorization (SPEC §6.3).

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query as AxumQuery, State};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::routing::{get, post};
use axum::{Json, http::StatusCode};
use chrono::{DateTime, Utc};
use futures_core::Stream;
use hivemind_core::index::{Query, Summary};
use hivemind_core::message::{Kind, Message, Recipient, SenderKind};
use hivemind_core::store::Mailbox;
use serde::{Deserialize, Serialize};
use ulid::Ulid;
use utoipa::{IntoParams, ToSchema};

use crate::problem::Problem;
use crate::service::{Draft, MailService};

/// Shared state for the loopback router.
pub type AppState = Arc<MailService>;

/// Who this node is (SPEC §7.1).
#[derive(Debug, Serialize, ToSchema)]
pub struct Me {
    /// This node's fingerprint, in its display form.
    #[schema(example = "hm1:w2mq-xor2-seiv-lghq-ybqb-36iv-i24o-n5vk-dpww-736k-eici-imu3-bnwq")]
    pub id: String,
    /// The short form people compare by eye.
    #[schema(example = "w2mqxor2")]
    pub short_id: String,
    /// The running version.
    pub version: String,
    /// How many unread messages there are.
    pub unread: u64,
    /// The display name peers see.
    pub name: String,
    /// The human who owns this machine, if they said.
    pub owner: Option<String>,
    /// The port the peer listener is on.
    pub peer_port: u16,
    /// How many peers are paired.
    pub peers: usize,
    /// How many introductions are waiting for a confirmation (SPEC §6.2).
    pub pending_pairs: usize,
    /// How many messages are still on their way out.
    pub outbox: usize,
}

/// A peer, paired or merely seen (SPEC §7.1).
#[derive(Debug, Serialize, ToSchema)]
pub struct PeerSummary {
    /// Its fingerprint, in display form.
    pub id: String,
    /// The short form people compare by eye.
    pub short_id: String,
    /// What it calls itself. Unverified (ADR 0003).
    pub name: String,
    /// Who owns it, if it said. Also unverified.
    pub owner: Option<String>,
    /// Where it can be reached, best guess first.
    pub addrs: Vec<String>,
    /// Whether mail will actually flow, or it is still waiting on a
    /// confirmation from one side or the other (SPEC §6.2).
    pub paired: bool,
    /// When this side confirmed. `null` while still pending.
    pub paired_at: Option<String>,
    /// When we last heard from it.
    pub last_seen: Option<String>,
}

impl From<hivemind_core::peerbook::Peer> for PeerSummary {
    fn from(peer: hivemind_core::peerbook::Peer) -> Self {
        Self {
            id: peer.id.to_string(),
            short_id: peer.id.short(),
            addrs: peer
                .addrs_by_preference()
                .into_iter()
                .map(hivemind_core::peerbook::PeerAddr::authority)
                .collect(),
            paired: true,
            paired_at: Some(peer.paired_at.to_rfc3339()),
            last_seen: peer.last_seen.map(|t| t.to_rfc3339()),
            name: peer.name,
            owner: peer.owner,
        }
    }
}

impl From<hivemind_core::peerbook::PendingPair> for PeerSummary {
    fn from(pending: hivemind_core::peerbook::PendingPair) -> Self {
        Self {
            id: pending.id.to_string(),
            short_id: pending.id.short(),
            addrs: vec![pending.addr.authority()],
            paired: false,
            // Not paired from this side, whatever the other side has done.
            paired_at: None,
            last_seen: Some(pending.first_seen.to_rfc3339()),
            name: pending.name,
            owner: pending.owner,
        }
    }
}

/// What a discovery run turned up.
#[derive(Debug, Serialize, ToSchema)]
pub struct Refreshed {
    /// How many nodes answered and were greeted.
    pub found: usize,
}

/// Where to look for a node to introduce ourselves to.
#[derive(Debug, Deserialize, ToSchema)]
pub struct JoinRequest {
    /// A hostname or address, with an optional `:port`.
    #[schema(example = "laptop.local:8400")]
    pub host: String,
}

/// One row of a listing (SPEC §9.1).
#[derive(Debug, Serialize, ToSchema)]
pub struct MessageSummary {
    /// The message id.
    pub id: String,
    /// The thread it belongs to.
    pub thread_id: String,
    /// Who sent it.
    pub from: String,
    /// Its subject.
    pub subject: String,
    /// `message`, `task` or `notification`.
    pub kind: String,
    /// `human` if a person typed it, `agent` if a Claude sent it.
    pub sender_kind: String,
    /// When it was sent.
    pub sent_at: DateTime<Utc>,
    /// Which mailbox it is in.
    pub mailbox: String,
    /// Whether it is still unread.
    pub unread: bool,
    /// The names of its attachments.
    pub attachment_names: Vec<String>,
}

impl From<Summary> for MessageSummary {
    fn from(s: Summary) -> Self {
        // Read the derived flag before the struct is torn apart.
        let unread = s.is_unread();
        Self {
            id: s.id.to_string(),
            thread_id: s.thread_id.to_string(),
            from: s.from.to_string(),
            subject: s.subject,
            kind: s.kind.as_str().to_owned(),
            sender_kind: s.sender_kind.as_str().to_owned(),
            sent_at: s.sent_at,
            mailbox: s.mailbox.as_str().to_owned(),
            unread,
            attachment_names: s.attachment_names,
        }
    }
}

/// A whole message.
#[derive(Debug, Serialize, ToSchema)]
pub struct MessageBody {
    /// The message id.
    pub id: String,
    /// The thread it belongs to.
    pub thread_id: String,
    /// What it replies to, if anything.
    pub in_reply_to: Option<String>,
    /// Who sent it.
    pub from: String,
    /// Its subject.
    pub subject: String,
    /// Its body, as markdown.
    pub body: String,
    /// `message`, `task` or `notification`.
    pub kind: String,
    /// `human` if a person typed it, `agent` if a Claude sent it.
    pub sender_kind: String,
    /// When it was sent.
    pub sent_at: DateTime<Utc>,
    /// When this node received it, if it did.
    pub received_at: Option<DateTime<Utc>>,
    /// Which mailbox it is in.
    pub mailbox: String,
    /// The files that came with it.
    pub attachments: Vec<Attachment>,
}

/// One attachment, as the local API describes it (SPEC §7.1).
#[derive(Debug, Serialize, ToSchema)]
pub struct Attachment {
    /// The name to save it as. Checked on arrival, never a path (SPEC §6.3).
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its content address, and the `sha` in the attachment URL.
    pub sha256: String,
    /// What the sender says it is. Advisory (SPEC §4.1).
    pub mime: String,
    /// Whether it travelled with the message or is fetched on demand.
    pub inline: bool,
    /// Whether the bytes are already on this machine. `false` means the first
    /// read of it will go to the sender.
    pub cached: bool,
}

impl MessageBody {
    fn new(mailbox: Mailbox, message: Message, blobs: &hivemind_core::blobs::BlobStore) -> Self {
        Self {
            id: message.id.to_string(),
            thread_id: message.thread_id.to_string(),
            in_reply_to: message.in_reply_to.map(|u| u.to_string()),
            from: message.from.to_string(),
            subject: message.subject,
            body: message.body,
            kind: message.kind.as_str().to_owned(),
            sender_kind: message.sender_kind.as_str().to_owned(),
            sent_at: message.sent_at,
            received_at: message.received_at,
            mailbox: mailbox.as_str().to_owned(),
            attachments: message
                .attachments
                .into_iter()
                .map(|a| Attachment {
                    // Whether the bytes are here is asked now rather than
                    // stored: a lazy attachment becomes cached the moment
                    // somebody reads it.
                    cached: blobs.has(&a.sha256),
                    sha256: a.sha256.to_hex(),
                    name: a.name,
                    size: a.size,
                    mime: a.mime,
                    inline: a.inline,
                })
                .collect(),
        }
    }
}

/// What to send.
///
/// There is deliberately no `sender_kind`: it is decided by which entrypoint
/// the request arrived through, and a caller offering one is ignored
/// (SPEC §4.1).
#[derive(Debug, Deserialize, ToSchema)]
pub struct SendRequest {
    /// Recipients: a node id, an owner name, or `everyone`.
    #[schema(example = json!(["everyone"]))]
    pub to: Vec<String>,
    /// The subject, at most 200 characters.
    pub subject: String,
    /// The body, as markdown.
    pub body: String,
    /// `message`, `task` or `notification`. Defaults to `message`.
    pub kind: Option<String>,
    /// Absolute paths to local files to send with it. The API is loopback
    /// only, so these are paths on this machine (SPEC §7.1).
    pub attachments: Option<Vec<String>>,
}

/// What to say in a reply.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReplyRequest {
    /// The body, as markdown.
    pub body: String,
    /// Absolute paths to local files to send with it. The API is loopback
    /// only, so these are paths on this machine (SPEC §7.1).
    pub attachments: Option<Vec<String>>,
}

/// What a send returns (SPEC §8: accepted, not delivered).
#[derive(Debug, Serialize, ToSchema)]
pub struct Accepted {
    /// The new message's id.
    pub id: String,
    /// The thread it belongs to.
    pub thread_id: String,
}

/// Listing filters (SPEC §7.1).
#[derive(Debug, Default, Deserialize, IntoParams)]
#[serde(default)]
pub struct ListParams {
    /// Restrict to one mailbox: `new`, `cur`, `out` or `sent`.
    pub r#box: Option<String>,
    /// Restrict to one thread.
    pub thread: Option<String>,
    /// Restrict to one sender, by node id.
    pub from: Option<String>,
    /// Only unread messages.
    pub unread: Option<bool>,
    /// Full-text search over subject and body.
    pub q: Option<String>,
    /// How many to return.
    pub limit: Option<usize>,
    /// Continue after this row, from a previous page (SPEC §7.1).
    ///
    /// Build it from the last summary already received: `<sent_at in
    /// milliseconds>:<id>`. Keyset rather than offset, so a message arriving
    /// while somebody pages through their inbox cannot make a row appear
    /// twice or not at all.
    pub cursor: Option<String>,
}

/// Turn the paths a local caller supplied into real ones.
///
/// Nothing is validated here: whether a path exists, is readable and fits
/// under the size limit is the blob store's answer to give, with a message the
/// caller can act on.
fn local_paths(paths: Option<Vec<String>>) -> Vec<std::path::PathBuf> {
    paths
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect()
}

/// Build the loopback router, including the Swagger UI at `/docs` (SPEC §7).
pub fn router(state: AppState) -> Router {
    use utoipa::OpenApi as _;

    Router::new()
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs")
                .url("/openapi.json", crate::openapi::ApiDoc::openapi()),
        )
        .route("/api/v1/me", get(me))
        .route("/api/v1/peers", get(list_peers))
        .route("/api/v1/peers/join", post(join_peer))
        .route("/api/v1/peers/refresh", post(refresh_peers))
        .route("/api/v1/peers/trust-network", post(trust_network))
        .route("/api/v1/peers/{id}", axum::routing::delete(remove_peer))
        .route("/api/v1/peers/{id}/pair", post(confirm_pair))
        .route("/api/v1/messages", get(list_messages).post(send_message))
        .route("/api/v1/messages/{id}", get(get_message))
        .route("/api/v1/messages/{id}/reply", post(reply_to_message))
        .route("/api/v1/messages/{id}/read", post(mark_read))
        .route(
            "/api/v1/messages/{id}/attachments/{sha}",
            get(get_attachment),
        )
        .route("/api/v1/threads/{thread_id}", get(get_thread))
        .route("/api/v1/events", get(events))
        .route("/healthz", get(healthz))
        .with_state(Arc::clone(&state))
        // Merged after the API's state is applied: the web router carries its
        // own, so the two cannot share one `with_state` call (SPEC §11).
        .merge(crate::web::router(state))
}

/// Liveness. Deliberately says nothing about the network.
#[utoipa::path(get, path = "/healthz", responses((status = 200, description = "The daemon is up")))]
pub(crate) async fn healthz() -> &'static str {
    "ok"
}

#[utoipa::path(
    get, path = "/api/v1/me",
    responses((status = 200, body = Me), (status = 500, body = Problem))
)]
pub(crate) async fn me(State(service): State<AppState>) -> Result<Json<Me>, Problem> {
    let id = service.identity();
    Ok(Json(Me {
        id: id.to_string(),
        short_id: id.short(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        unread: service.unread_count()?,
        name: service.name().to_owned(),
        owner: service.owner().map(ToOwned::to_owned),
        peer_port: service.peer_port(),
        peers: service.paired_peers()?.len(),
        pending_pairs: service.pending_pairs()?.len(),
        outbox: service.pending_outbound()?.len(),
    }))
}

#[utoipa::path(
    get, path = "/api/v1/messages/{id}/attachments/{sha}",
    params(
        ("id" = String, Path, description = "The message the attachment belongs to"),
        ("sha" = String, Path, description = "The attachment's SHA-256, in hex")
    ),
    responses(
        (status = 200, description = "The attachment's bytes", content_type = "application/octet-stream"),
        (status = 404, body = Problem),
        (status = 502, body = Problem)
    ),
    tag = "messages"
)]
pub(crate) async fn get_attachment(
    State(service): State<AppState>,
    Path((id, sha)): Path<(String, String)>,
) -> Result<axum::response::Response, Problem> {
    let id = parse_id(&id)?;
    let digest: hivemind_core::crypto::Sha256Digest = sha.parse().map_err(|_| {
        Problem::new(
            crate::problem::ProblemType::BlobNotFound,
            "that is not a SHA-256 digest, so there is no such attachment",
        )
    })?;

    // Blocks until the fetch finishes on a first access (SPEC §7.1). A
    // progress stream would be nicer; a partial file served as if whole would
    // be worse than waiting.
    let path = service.fetch_attachment(id, digest).await?;
    let (_, message) = service.get(id)?;
    let attachment = message
        .attachments
        .iter()
        .find(|a| a.sha256 == digest)
        .ok_or_else(|| {
            Problem::new(
                crate::problem::ProblemType::BlobNotFound,
                "this message has no such attachment",
            )
        })?;

    let file = tokio::fs::File::open(&path).await.map_err(|_| {
        Problem::new(
            crate::problem::ProblemType::BlobNotFound,
            "the attachment is no longer on disk",
        )
    })?;

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, attachment.mime.clone())
        .header(axum::http::header::CONTENT_LENGTH, attachment.size)
        // The name came from another machine and is checked on arrival, but it
        // is quoted here too: a header is not the place to find out.
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", attachment.name),
        )
        .body(axum::body::Body::from_stream(
            tokio_util::io::ReaderStream::new(file),
        ))
        .map_err(|_| {
            Problem::new(
                crate::problem::ProblemType::Internal,
                "could not build a response",
            )
        })
}

#[utoipa::path(
    get, path = "/api/v1/peers",
    responses((status = 200, body = Vec<PeerSummary>), (status = 500, body = Problem)),
    tag = "peers"
)]
pub(crate) async fn list_peers(
    State(service): State<AppState>,
) -> Result<Json<Vec<PeerSummary>>, Problem> {
    // Paired and merely-seen in one list, because "who can I mail?" and "who
    // is waiting on me?" are the same question asked at different moments.
    let mut peers: Vec<PeerSummary> = service
        .paired_peers()?
        .into_iter()
        .map(PeerSummary::from)
        .collect();
    peers.extend(service.pending_pairs()?.into_iter().map(PeerSummary::from));

    Ok(Json(peers))
}

#[utoipa::path(
    post, path = "/api/v1/peers/join",
    request_body = JoinRequest,
    responses(
        (status = 200, body = PeerSummary),
        (status = 400, body = Problem),
        (status = 502, body = Problem)
    ),
    tag = "peers"
)]
pub(crate) async fn join_peer(
    State(service): State<AppState>,
    Json(request): Json<JoinRequest>,
) -> Result<Json<PeerSummary>, Problem> {
    let pending = service.join(&request.host).await?;
    Ok(Json(PeerSummary::from(pending)))
}

#[utoipa::path(
    post, path = "/api/v1/peers/refresh",
    responses((status = 200, body = Refreshed), (status = 500, body = Problem)),
    tag = "peers"
)]
pub(crate) async fn refresh_peers(
    State(service): State<AppState>,
) -> Result<Json<Refreshed>, Problem> {
    Ok(Json(Refreshed {
        found: service.refresh_peers().await?,
    }))
}

#[utoipa::path(
    post, path = "/api/v1/peers/trust-network",
    responses((status = 200, body = Vec<PeerSummary>), (status = 500, body = Problem)),
    tag = "peers"
)]
pub(crate) async fn trust_network(
    State(service): State<AppState>,
) -> Result<Json<Vec<PeerSummary>>, Problem> {
    Ok(Json(
        service
            .confirm_all_discovered()?
            .into_iter()
            .map(PeerSummary::from)
            .collect(),
    ))
}

#[utoipa::path(
    post, path = "/api/v1/peers/{id}/pair",
    params(("id" = String, Path, description = "The peer's full or short id")),
    responses((status = 200, body = PeerSummary), (status = 403, body = Problem)),
    tag = "peers"
)]
pub(crate) async fn confirm_pair(
    State(service): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<PeerSummary>, Problem> {
    let peer = service.confirm_pair(service.resolve_peer(&id)?)?;
    Ok(Json(PeerSummary::from(peer)))
}

#[utoipa::path(
    delete, path = "/api/v1/peers/{id}",
    params(("id" = String, Path, description = "The peer's full or short id")),
    responses((status = 204, description = "Forgotten"), (status = 403, body = Problem)),
    tag = "peers"
)]
pub(crate) async fn remove_peer(
    State(service): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    service.remove_peer(service.resolve_peer(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/api/v1/messages",
    params(ListParams),
    responses((status = 200, body = Vec<MessageSummary>), (status = 500, body = Problem))
)]
pub(crate) async fn list_messages(
    State(service): State<AppState>,
    AxumQuery(params): AxumQuery<ListParams>,
) -> Result<Json<Vec<MessageSummary>>, Problem> {
    let query = Query {
        // An unknown mailbox name filters to nothing rather than erroring: it
        // is a typo in a query string, not a broken client.
        mailbox: params.r#box.as_deref().and_then(Mailbox::from_str_opt),
        thread: params.thread.as_deref().and_then(|t| t.parse().ok()),
        from: params.from.as_deref().and_then(|f| f.parse().ok()),
        unread_only: params.unread.unwrap_or(false),
        text: params.q,
        limit: params.limit,
        // A cursor this version did not issue filters to nothing rather than
        // erroring, the same reading as an unknown mailbox above: it is a
        // stale query string, not a broken client.
        cursor: params.cursor.as_deref().and_then(|c| c.parse().ok()),
    };

    Ok(Json(
        service
            .list(&query)?
            .into_iter()
            .map(MessageSummary::from)
            .collect(),
    ))
}

#[utoipa::path(
    get, path = "/api/v1/messages/{id}",
    params(("id" = String, Path, description = "Message id")),
    responses((status = 200, body = MessageBody), (status = 404, body = Problem))
)]
pub(crate) async fn get_message(
    State(service): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<MessageBody>, Problem> {
    let id = parse_id(&id)?;
    let (mailbox, message) = service.get(id)?;
    Ok(Json(MessageBody::new(mailbox, message, service.blobs())))
}

#[utoipa::path(
    post, path = "/api/v1/messages",
    request_body = SendRequest,
    responses((status = 202, body = Accepted), (status = 422, body = Problem))
)]
pub(crate) async fn send_message(
    State(service): State<AppState>,
    Json(request): Json<SendRequest>,
) -> Result<(StatusCode, Json<Accepted>), Problem> {
    let draft = Draft {
        to: request.to.iter().map(|s| parse_recipient(s)).collect(),
        subject: request.subject,
        body: request.body,
        kind: request
            .kind
            .as_deref()
            .and_then(Kind::from_str_opt)
            .unwrap_or(Kind::Message),
        in_reply_to: None,
        attachments: local_paths(request.attachments),
    };

    // HTTP means a human at the CLI or the web UI. MCP sets Agent instead, and
    // neither lets the caller choose (SPEC §4.1).
    let message = service.send(draft, SenderKind::Human)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            id: message.id.to_string(),
            thread_id: message.thread_id.to_string(),
        }),
    ))
}

#[utoipa::path(
    post, path = "/api/v1/messages/{id}/reply",
    params(("id" = String, Path, description = "Message being replied to")),
    request_body = ReplyRequest,
    responses((status = 202, body = Accepted), (status = 404, body = Problem))
)]
pub(crate) async fn reply_to_message(
    State(service): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ReplyRequest>,
) -> Result<(StatusCode, Json<Accepted>), Problem> {
    let id = parse_id(&id)?;
    let message = service.reply(
        id,
        request.body,
        local_paths(request.attachments),
        SenderKind::Human,
    )?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            id: message.id.to_string(),
            thread_id: message.thread_id.to_string(),
        }),
    ))
}

#[utoipa::path(
    post, path = "/api/v1/messages/{id}/read",
    params(("id" = String, Path, description = "Message id")),
    responses((status = 204, description = "Marked read"), (status = 404, body = Problem))
)]
pub(crate) async fn mark_read(
    State(service): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    service.mark_read(parse_id(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/api/v1/threads/{thread_id}",
    params(("thread_id" = String, Path, description = "Thread id")),
    responses((status = 200, body = Vec<MessageSummary>), (status = 404, body = Problem))
)]
pub(crate) async fn get_thread(
    State(service): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<MessageSummary>>, Problem> {
    let thread_id = parse_id(&thread_id)?;
    Ok(Json(
        service
            .thread(thread_id)?
            .into_iter()
            .map(MessageSummary::from)
            .collect(),
    ))
}

/// The SSE stream (SPEC §7.1).
#[utoipa::path(
    get, path = "/api/v1/events",
    responses((status = 200, description = "text/event-stream of message.* events"))
)]
pub(crate) async fn events(
    State(service): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let receiver = service.subscribe();
    let stream = async_stream::stream! {
        let mut receiver = receiver;
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    yield Ok(SseEvent::default()
                        .event(event.name())
                        .data(event.id().to_string()));
                }
                // A subscriber that fell behind has missed events it can never
                // get back. Keep the stream open: the client re-reads the
                // inbox rather than losing the connection.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

/// Serve the loopback router until `shutdown` resolves.
///
/// Takes a bound listener rather than an address so the caller decides where to
/// bind — and so a test can bind port 0 and find out what it got.
///
/// # Errors
/// Returns whatever the server failed with.
pub async fn serve<F>(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

fn parse_id(raw: &str) -> Result<Ulid, Problem> {
    raw.parse().map_err(|_| {
        Problem::new(
            crate::problem::ProblemType::MessageNotFound,
            format!("`{raw}` is not a message id"),
        )
    })
}

/// A recipient string is a node id, `everyone`, or an owner name (SPEC §9.1).
fn parse_recipient(raw: &str) -> Recipient {
    if raw.eq_ignore_ascii_case("everyone") {
        return Recipient::Everyone;
    }
    match raw.parse() {
        Ok(node) => Recipient::Node(node),
        Err(_) => Recipient::Owner(raw.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hivemind_core::crypto::SigningKey;
    use hivemind_core::peer::NodeId;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    fn app() -> (tempfile::TempDir, Router, NodeId) {
        let dir = tempfile::tempdir().expect("temp dir");
        let identity = NodeId::from_certificate_der(b"this node");
        let node = crate::service::NodeDescription {
            id: identity,
            certificate: b"this node".to_vec(),
            private_key: Vec::new(),
            name: "test".to_owned(),
            owner: None,
            callback_host: "127.0.0.1".to_owned(),
            peer_port: 8400,
            max_attachment_bytes: hivemind_core::config::DEFAULT_MAX_ATTACHMENT_BYTES,
            inline_max_bytes: hivemind_core::config::DEFAULT_INLINE_MAX_BYTES,
            prefetch: false,
        };
        let service = MailService::open(dir.path(), node, SigningKey::from_bytes(&[11u8; 32]))
            .expect("service");
        let router = router(Arc::new(service));
        (dir, router, identity)
    }

    async fn call(router: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("router responds");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("request")
    }

    fn post_json(path: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    #[tokio::test]
    async fn healthz_says_ok() {
        let (_dir, router, _) = app();
        let response = router.oneshot(get("/healthz")).await.expect("respond");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn me_reports_this_nodes_identity_and_unread_count() {
        let (_dir, router, identity) = app();
        let (status, body) = call(&router, get("/api/v1/me")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], identity.to_string());
        assert_eq!(body["short_id"], identity.short());
        assert_eq!(body["unread"], 0);
    }

    #[tokio::test]
    async fn sending_returns_202_accepted_not_200() {
        // SPEC §8: the message is written to out/ and the call returns. It has
        // been accepted, not delivered, and the status should not claim more.
        let (_dir, router, identity) = app();
        let (status, body) = call(
            &router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({
                    "to": [identity.to_string()],
                    "subject": "dashboard PR",
                    "body": "take a look",
                }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body["id"].is_string());
        assert_eq!(
            body["id"], body["thread_id"],
            "a new message starts a thread"
        );
    }

    #[tokio::test]
    async fn a_message_sent_over_http_is_marked_as_written_by_a_human() {
        // SPEC §4.1: the entrypoint decides. HTTP is the CLI and the web UI.
        let (_dir, router, identity) = app();
        call(
            &router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({
                    "to": [identity.to_string()],
                    "subject": "typed by a person",
                    "body": "x",
                }),
            ),
        )
        .await;

        let (_, list) = call(&router, get("/api/v1/messages?box=new")).await;
        assert_eq!(list[0]["sender_kind"], "human");
    }

    #[tokio::test]
    async fn a_caller_cannot_claim_to_be_a_human_by_sending_the_field() {
        let (_dir, router, identity) = app();
        let (status, _) = call(
            &router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({
                    "to": [identity.to_string()],
                    "subject": "x",
                    "body": "y",
                    "sender_kind": "agent",
                }),
            ),
        )
        .await;

        // The unknown field is ignored rather than honoured.
        assert_eq!(status, StatusCode::ACCEPTED);
        let (_, list) = call(&router, get("/api/v1/messages?box=new")).await;
        assert_eq!(list[0]["sender_kind"], "human");
    }

    #[tokio::test]
    async fn a_sent_message_can_be_listed_read_and_marked_read() {
        let (_dir, router, identity) = app();
        let (_, accepted) = call(
            &router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({
                    "to": [identity.to_string()],
                    "subject": "round trip",
                    "body": "the whole of M1",
                }),
            ),
        )
        .await;
        let id = accepted["id"].as_str().expect("id").to_owned();

        let (status, listed) = call(&router, get("/api/v1/messages?unread=true")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed.as_array().expect("array").len(), 1);
        assert_eq!(listed[0]["unread"], true);

        let (status, message) = call(&router, get(&format!("/api/v1/messages/{id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(message["body"], "the whole of M1");

        let response = router
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/messages/{id}/read"),
                &serde_json::json!({}),
            ))
            .await
            .expect("respond");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let (_, me) = call(&router, get("/api/v1/me")).await;
        assert_eq!(me["unread"], 0);
    }

    #[tokio::test]
    async fn replying_over_http_keeps_the_thread() {
        let (_dir, router, identity) = app();
        let (_, accepted) = call(
            &router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({
                    "to": [identity.to_string()],
                    "subject": "lunch",
                    "body": "?",
                }),
            ),
        )
        .await;
        let id = accepted["id"].as_str().expect("id").to_owned();
        let thread_id = accepted["thread_id"].as_str().expect("thread").to_owned();

        let (status, reply) = call(
            &router,
            post_json(
                &format!("/api/v1/messages/{id}/reply"),
                &serde_json::json!({ "body": "1pm" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(reply["thread_id"], thread_id);

        let (_, thread) = call(&router, get(&format!("/api/v1/threads/{thread_id}"))).await;
        assert!(thread.as_array().expect("array").len() >= 2);
    }

    #[tokio::test]
    async fn asking_for_a_message_that_does_not_exist_is_a_problem_json_404() {
        let (_dir, router, _) = app();
        let missing = Ulid::generate();
        let response = router
            .clone()
            .oneshot(get(&format!("/api/v1/messages/{missing}")))
            .await
            .expect("respond");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
            "SPEC §7.3 asks for problem+json everywhere"
        );

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let problem: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(problem["type"], "/problems/message-not-found");
        assert_eq!(problem["status"], 404);
        assert!(problem["title"].is_string());
    }

    #[tokio::test]
    async fn a_malformed_message_id_is_a_404_not_a_500() {
        let (_dir, router, _) = app();
        let (status, problem) = call(&router, get("/api/v1/messages/not-a-ulid")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(problem["type"], "/problems/message-not-found");
    }

    #[tokio::test]
    async fn a_message_with_no_recipients_is_422_with_its_own_slug() {
        let (_dir, router, _) = app();
        let (status, problem) = call(
            &router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({ "to": [], "subject": "x", "body": "y" }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem["type"], "/problems/no-recipients");
    }

    #[tokio::test]
    async fn an_oversized_subject_is_422_with_its_own_slug() {
        let (_dir, router, identity) = app();
        let (status, problem) = call(
            &router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({
                    "to": [identity.to_string()],
                    "subject": "s".repeat(201),
                    "body": "y",
                }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem["type"], "/problems/invalid-message");
    }

    #[tokio::test]
    async fn an_unknown_mailbox_filter_returns_nothing_rather_than_failing() {
        let (_dir, router, _) = app();
        let (status, body) = call(&router, get("/api/v1/messages?box=nonsense")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().expect("array").len(), 0);
    }

    #[tokio::test]
    async fn full_text_search_works_through_the_query_string() {
        let (_dir, router, identity) = app();
        for subject in ["dashboard PR", "lunch"] {
            call(
                &router,
                post_json(
                    "/api/v1/messages",
                    &serde_json::json!({
                        "to": [identity.to_string()],
                        "subject": subject,
                        "body": "x",
                    }),
                ),
            )
            .await;
        }

        let (status, found) = call(&router, get("/api/v1/messages?q=dashboard&box=new")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(found.as_array().expect("array").len(), 1);
        assert_eq!(found[0]["subject"], "dashboard PR");
    }

    #[test]
    fn everyone_is_recognised_whatever_its_case() {
        assert_eq!(parse_recipient("everyone"), Recipient::Everyone);
        assert_eq!(parse_recipient("EVERYONE"), Recipient::Everyone);
    }

    #[test]
    fn a_recipient_that_is_not_a_node_id_is_read_as_an_owner_name() {
        assert_eq!(
            parse_recipient("rafael"),
            Recipient::Owner("rafael".to_owned())
        );
    }

    #[test]
    fn a_node_id_recipient_is_read_as_a_node() {
        let id = NodeId::from_certificate_der(b"somebody");
        assert_eq!(parse_recipient(&id.to_string()), Recipient::Node(id));
    }

    /// Ask for a path and report the status and body.
    ///
    /// Used by the contract test below, which cares that a route exists and
    /// was reached — not what it answers.
    async fn touch(router: &Router, method: &str, path: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .expect("request");

        // Bounded, because `/api/v1/events` is a stream that by design never
        // ends: collecting its body waits for a shutdown that is not coming.
        // Reaching the handler is what this asks, so the timeout is the
        // answer rather than a failure.
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            router.clone().oneshot(request),
        )
        .await
        .expect("the route answered within five seconds")
        .expect("response");

        let status = response.status();
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            response.into_body().collect(),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(|collected| String::from_utf8_lossy(&collected.to_bytes()).into_owned())
        .unwrap_or_default();

        (status, body)
    }

    #[tokio::test]
    async fn every_endpoint_in_the_openapi_document_is_routed() {
        // SPEC §13.2 asks that every endpoint in `openapi.json` have at least
        // one test. This is the half a person cannot forget: a path documented
        // and never routed answers 404 to a client that read the document and
        // believed it.
        //
        // `openapi-check` already fails if the document drifts from the code
        // that generates it. This is the other direction — that what the
        // document promises is actually reachable.
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/openapi.json"))
                .expect("the checked-in document parses");
        let paths = document["paths"].as_object().expect("a paths object");
        assert!(
            paths.len() >= 15,
            "expected the full API, got {}",
            paths.len()
        );

        let (_dir, router, identity) = app();

        for (path, methods) in paths {
            // utoipa writes `{id}`; a request needs something that parses.
            let concrete = path
                .replace("{id}", &ulid::Ulid::generate().to_string())
                .replace("{thread_id}", &ulid::Ulid::generate().to_string())
                .replace("{sha}", &"ab".repeat(32))
                .replace("{file}", "hivemind.css");

            for method in methods.as_object().expect("methods").keys() {
                let (status, body) = touch(&router, &method.to_uppercase(), &concrete).await;

                assert_ne!(
                    status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path} is in openapi.json and the route refuses that method"
                );

                // A 404 is two different answers wearing one number. axum's —
                // no such route — has an empty body; a handler's is problem+json
                // naming the thing that was not found (SPEC §7.3). Only the
                // first is a broken promise, and the difference is the body.
                if status == StatusCode::NOT_FOUND {
                    assert!(
                        body.contains("/problems/"),
                        "{method} {path} is in openapi.json and nothing is routed there"
                    );
                }
            }
        }
        // Unused unless a route above needs it; keeps the helper honest.
        let _ = identity;
    }

    #[tokio::test]
    async fn a_listing_can_be_paged_with_a_cursor() {
        // SPEC §7.1's `cursor=`, through the query string a client would use.
        let (_dir, router, identity) = app();
        for n in 1..=3 {
            call(
                &router,
                post_json(
                    "/api/v1/messages",
                    &serde_json::json!({
                        "to": [identity.to_string()],
                        "subject": format!("m{n}"),
                        "body": "x",
                    }),
                ),
            )
            .await;
        }

        let (_, first) = call(&router, get("/api/v1/messages?box=new&limit=2")).await;
        let rows = first.as_array().expect("an array");
        assert_eq!(rows.len(), 2);

        let last = &rows[1];
        let cursor = format!(
            "{}:{}",
            chrono::DateTime::parse_from_rfc3339(last["sent_at"].as_str().expect("a time"))
                .expect("rfc3339")
                .timestamp_millis(),
            last["id"].as_str().expect("an id")
        );

        let (_, second) = call(
            &router,
            get(&format!("/api/v1/messages?box=new&limit=2&cursor={cursor}")),
        )
        .await;
        let rows = second.as_array().expect("an array");
        assert_eq!(rows.len(), 1, "one left after the first page");
        assert_ne!(rows[0]["id"], last["id"], "and not the one we already had");
    }

    #[tokio::test]
    async fn a_cursor_this_version_did_not_issue_is_ignored_rather_than_fatal() {
        // A stale query string in somebody's history is not a broken client.
        let (_dir, router, identity) = app();
        call(
            &router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({
                    "to": [identity.to_string()],
                    "subject": "only one",
                    "body": "x",
                }),
            ),
        )
        .await;

        let (status, body) = call(&router, get("/api/v1/messages?cursor=nonsense")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.as_array().expect("an array").len(),
            2,
            "it should read as no cursor at all: the message in new and in sent"
        );
    }
}
