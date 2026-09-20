//! The loopback router (SPEC §7.1).
//!
//! Binds to `127.0.0.1` and fails closed if configured otherwise: it carries no
//! authentication, so reachability *is* the authorization (SPEC §6.3).
//!
//! This module hands out a [`router`] and deliberately does not serve it. The
//! daemon calls `axum::serve` itself: it nests the MCP service at `/mcp`, which
//! this crate cannot depend on (SPEC §3), and a graceful shutdown has to close
//! the event streams and the MCP sessions *before* draining, or `/api/v1/events`
//! keeps the process alive for ever (#108). A helper here that took a shutdown
//! future and forgot that was #112.

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
use hivemind_core::message::{Kind, Message, SenderKind};
use hivemind_core::store::Mailbox;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::problem::Problem;
use crate::service::{Draft, MailService};

pub mod group;
pub mod sessions;
pub mod threads;

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
    /// When the binary this daemon is running was last written, RFC 3339.
    ///
    /// `null` when it could not be read. The version cannot answer "is this
    /// daemon the code that is installed" — it does not move between two builds
    /// of one release — and this can (#36).
    pub binary_modified_at: Option<String>,
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
    /// Whether this node is in a group at all (SPEC §6.2). Without one it
    /// can reach nobody.
    pub in_group: bool,
    /// How many nodes have been seen that are not in this node's group.
    pub seen: usize,
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
    /// Whether it has proved the group key, so mail flows (SPEC §6.2), or was
    /// only seen.
    pub paired: bool,
    /// When it proved the key to this node. `null` for a node only seen.
    pub paired_at: Option<String>,
    /// When we last heard from it.
    pub last_seen: Option<String>,
    /// Whether it said hello within the last two presence intervals
    /// (SPEC §5.5). Always `false` for a node only seen.
    pub online: bool,
    /// What it is working on: one label per open Claude Code session
    /// (SPEC §9.3). Empty until #52 fills the register.
    pub sessions: Vec<String>,
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
            // Presence is not a property of the address book, so a summary
            // built from a `Peer` alone cannot know it. `list_peers` fills
            // this in; everywhere else answers about pairing, not presence.
            online: false,
            sessions: Vec::new(),
        }
    }
}

impl From<crate::service::SeenNode> for PeerSummary {
    fn from(seen: crate::service::SeenNode) -> Self {
        Self {
            id: seen.id.to_string(),
            short_id: seen.id.short(),
            // A node that refused our handshake said nothing but its
            // certificate; where it was is the best name there is.
            name: seen.name.unwrap_or_else(|| seen.addr.host.clone()),
            addrs: vec![seen.addr.authority()],
            paired: false,
            paired_at: None,
            last_seen: Some(seen.last_seen.to_rfc3339()),
            owner: seen.owner,
            // A node that is not in the group does not say hello, so there is
            // nothing that could make this true.
            online: false,
            sessions: Vec::new(),
        }
    }
}

impl From<crate::service::Met> for PeerSummary {
    fn from(met: crate::service::Met) -> Self {
        match met {
            crate::service::Met::Member(peer) => peer.into(),
            crate::service::Met::Stranger(seen) => seen.into(),
        }
    }
}

/// What a discovery run turned up.
#[derive(Debug, Serialize, ToSchema)]
pub struct Refreshed {
    /// How many nodes answered, members or not.
    pub found: usize,
}

/// Where to look for a node discovery cannot find.
#[derive(Debug, Deserialize, ToSchema)]
pub struct JoinRequest {
    /// A hostname or address, with an optional `:port`.
    #[schema(example = "laptop.local:8400")]
    pub host: String,
}

/// Which of a peer's addresses to forget.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ForgetAddrRequest {
    /// The address, exactly as `hivemind peers` and `doctor` print it.
    #[schema(example = "127.0.0.1:8400")]
    pub addr: String,
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
    /// An identical message sent in the last two minutes, if there was one
    /// (#33). Absent when there was not. The message was still queued: this is
    /// a notice for whoever pressed send, not a refusal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
}

impl From<crate::service::Queued> for Accepted {
    fn from(queued: crate::service::Queued) -> Self {
        Self {
            id: queued.message.id.to_string(),
            thread_id: queued.message.thread_id.to_string(),
            duplicate_of: queued.duplicate_of.map(|id| id.to_string()),
        }
    }
}

/// Listing filters (SPEC §7.1).
///
/// `deny_unknown_fields` is the point of #28. `?mailbox=sent` instead of
/// `?box=sent` used to be accepted and ignored, so two different questions
/// gave the same answer and the Claude that typed it concluded the `out`
/// mailbox was broken. It was not; what was broken was the API not saying the
/// question made no sense.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[serde(default, deny_unknown_fields)]
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

/// Parse a filter that was given, or leave it unset.
///
/// The difference that mattered in #28: absent means "do not filter", but
/// **present and unparseable means the caller asked something that makes no
/// sense**, and answering it with an unfiltered list is a wrong answer that
/// looks like a right one.
fn parse_filter<T>(
    raw: Option<&str>,
    name: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>, Problem> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match parse(raw) {
        Some(value) => Ok(Some(value)),
        None => Err(Problem::new(
            crate::problem::ProblemType::InvalidQuery,
            match name {
                "box" => format!("`{raw}` is not a mailbox; try new, cur, out or sent"),
                other => format!("`{raw}` is not a valid `{other}`"),
            },
        )),
    }
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

    let router = Router::new()
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs")
                .url("/openapi.json", crate::openapi::ApiDoc::openapi()),
        )
        .route("/api/v1/me", get(me))
        .route("/api/v1/peers", get(list_peers))
        .route("/api/v1/peers/join", post(join_peer))
        .route("/api/v1/peers/refresh", post(refresh_peers))
        .route("/api/v1/peers/{id}", axum::routing::delete(remove_peer))
        .route("/api/v1/peers/{id}/forget-addr", post(forget_addr))
        .route("/api/v1/group", get(group::status))
        .route("/api/v1/group/create", post(group::create))
        .route("/api/v1/group/join", post(group::join))
        .route("/api/v1/messages", get(list_messages).post(send_message))
        .route("/api/v1/messages/{id}", get(get_message))
        .route("/api/v1/messages/{id}/reply", post(reply_to_message))
        .route("/api/v1/messages/{id}/read", post(mark_read))
        .route(
            "/api/v1/messages/{id}/attachments/{sha}",
            get(get_attachment),
        )
        .route("/api/v1/threads", get(threads::list_threads))
        .route("/api/v1/threads/{thread_id}", get(threads::get_thread))
        .route("/api/v1/sessions", get(sessions::list_sessions))
        .route(
            "/api/v1/sessions/{id}",
            post(sessions::register_session).delete(sessions::end_session),
        )
        .route("/api/v1/events", get(events))
        .route("/healthz", get(healthz))
        .with_state(Arc::clone(&state))
        // Merged after the API's state is applied: the web router carries its
        // own, so the two cannot share one `with_state` call (SPEC §11).
        .merge(crate::web::router(state));

    stamp_binary(router)
}

/// Say on every response when the running daemon's binary was written (#36).
///
/// A header rather than only the field on `/api/v1/me`, because the command
/// somebody is running when it matters is `hivemind inbox`, not `status` — and
/// a warning worth having is one that costs no extra round-trip to earn.
fn stamp_binary(router: Router) -> Router {
    let stamp = crate::freshness::started_from()
        .and_then(|when| axum::http::HeaderValue::from_str(&when.to_rfc3339()).ok());
    let name = axum::http::HeaderName::from_static(crate::freshness::BINARY_MODIFIED);

    router.layer(axum::middleware::map_response(
        move |mut response: axum::response::Response| {
            let (name, stamp) = (name.clone(), stamp.clone());
            async move {
                if let Some(stamp) = stamp {
                    response.headers_mut().insert(name, stamp);
                }
                response
            }
        },
    ))
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
        binary_modified_at: crate::freshness::started_from().map(|when| when.to_rfc3339()),
        unread: service.unread_count()?,
        name: service.name().to_owned(),
        owner: service.owner().map(ToOwned::to_owned),
        peer_port: service.peer_port(),
        peers: service.paired_peers()?.len(),
        in_group: service.in_group()?,
        seen: service.seen_nodes()?.len(),
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
    let id = service.resolve_message(&id)?;
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
    // Members and merely-seen in one list, because "who can I mail?" and "why
    // can I not mail that machine?" are the same question asked at different
    // moments.
    let mut peers: Vec<PeerSummary> = service
        .paired_peers()?
        .into_iter()
        .map(|peer| {
            // SPEC §5.5: online and the session labels come from presence,
            // which the address book knows nothing about.
            let presence = service.presence_of(peer.id);
            let mut summary = PeerSummary::from(peer);
            if let Some(presence) = presence {
                summary.online = true;
                summary.sessions = presence
                    .sessions
                    .into_iter()
                    .map(|session| session.label)
                    .collect();
            }
            summary
        })
        .collect();
    peers.extend(service.seen_nodes()?.into_iter().map(PeerSummary::from));

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
    Ok(Json(PeerSummary::from(service.join(&request.host).await?)))
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
    post, path = "/api/v1/peers/{id}/forget-addr",
    params(("id" = String, Path, description = "The peer's full or short id")),
    request_body = ForgetAddrRequest,
    responses(
        (status = 200, body = PeerSummary),
        (status = 403, body = Problem),
        (status = 404, body = Problem)
    ),
    tag = "peers"
)]
pub(crate) async fn forget_addr(
    State(service): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ForgetAddrRequest>,
) -> Result<Json<PeerSummary>, Problem> {
    let id = service.resolve_peer(&id)?;
    Ok(Json(PeerSummary::from(
        service.forget_addr(id, &request.addr)?,
    )))
}

#[utoipa::path(
    get, path = "/api/v1/messages",
    params(ListParams),
    responses((status = 200, body = Vec<MessageSummary>), (status = 500, body = Problem))
)]
pub(crate) async fn list_messages(
    State(service): State<AppState>,
    params: Result<AxumQuery<ListParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<MessageSummary>>, Problem> {
    // The rejection carries serde's own message, which names the offending
    // parameter. Turned into problem+json here because axum's default is
    // plain text, and SPEC §7.3 says every error on this surface is
    // problem+json.
    let AxumQuery(params) = params.map_err(|rejection| {
        Problem::new(
            crate::problem::ProblemType::InvalidQuery,
            format!(
                "{}. Accepted: box, thread, from, unread, q, limit, cursor.",
                rejection.body_text()
            ),
        )
    })?;

    let query = Query {
        // A mailbox name that is not one is refused rather than ignored. It
        // used to filter to nothing in the comment and to *everything* in the
        // code — `?box=banana` returned the whole list with a 200 (#28).
        mailbox: parse_filter(params.r#box.as_deref(), "box", |value| {
            Mailbox::from_str_opt(value)
        })?,
        thread: parse_filter(params.thread.as_deref(), "thread", |t| t.parse().ok())?,
        from: parse_filter(params.from.as_deref(), "from", |f| f.parse().ok())?,
        unread_only: params.unread.unwrap_or(false),
        text: params.q,
        limit: params.limit,
        // A cursor this version did not issue still filters to nothing rather
        // than erroring, and that one is deliberate: a stale cursor comes
        // from a page somebody left open, not from a caller who got it wrong.
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
    let id = service.resolve_message(&id)?;
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
        to: request
            .to
            .iter()
            .map(|typed| service.parse_recipient(typed))
            .collect::<Result<Vec<_>, _>>()?,
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
    let queued = service.send(draft, SenderKind::Human)?;
    Ok((StatusCode::ACCEPTED, Json(queued.into())))
}

#[utoipa::path(
    post, path = "/api/v1/messages/{id}/reply",
    params(("id" = String, Path, description = "The message being answered, or a thread — which answers the most recent message in it")),
    request_body = ReplyRequest,
    responses((status = 202, body = Accepted), (status = 404, body = Problem))
)]
pub(crate) async fn reply_to_message(
    State(service): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ReplyRequest>,
) -> Result<(StatusCode, Json<Accepted>), Problem> {
    let id = service.resolve_message(&id)?;
    let queued = service.reply(
        id,
        request.body,
        local_paths(request.attachments),
        SenderKind::Human,
    )?;
    Ok((StatusCode::ACCEPTED, Json(queued.into())))
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
    service.mark_read(service.resolve_message(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The SSE stream (SPEC §7.1).
#[utoipa::path(
    get, path = "/api/v1/events",
    responses((status = 200, description = "text/event-stream of message.* and peer.* events; the data is the id the event is about"))
)]
pub(crate) async fn events(
    State(service): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let receiver = service.subscribe();
    let mut closing = service.closing();
    let stream = async_stream::stream! {
        let mut receiver = receiver;
        loop {
            let arrived = tokio::select! {
                arrived = receiver.recv() => arrived,
                // Ending the stream is what lets this connection drain, and a
                // graceful shutdown waits on every one that does not (#108).
                _ = closing.wait_for(|closing| *closing) => break,
            };
            match arrived {
                Ok(event) => {
                    yield Ok(SseEvent::default()
                        .event(event.name())
                        .data(event.data()));
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

#[cfg(test)]
mod message_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse as _;
    use hivemind_core::crypto::SigningKey;
    use hivemind_core::peer::NodeId;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    pub(super) fn app() -> (tempfile::TempDir, Router, NodeId) {
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
            presence_interval: hivemind_core::config::DEFAULT_PRESENCE_INTERVAL,
            tailscale: hivemind_core::config::Tailscale::Auto,
        };
        let service = MailService::open(dir.path(), node, SigningKey::from_bytes(&[11u8; 32]))
            .expect("service");
        let router = router(Arc::new(service));
        (dir, router, identity)
    }

    /// The same, keeping the service so a test can arrange state the HTTP
    /// surface has no way to reach — presence, for one: it is set by a hello
    /// arriving on the *peer* listener, which is not this router.
    pub(super) fn app_with_service() -> (tempfile::TempDir, Router, Arc<MailService>) {
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
            presence_interval: hivemind_core::config::DEFAULT_PRESENCE_INTERVAL,
            tailscale: hivemind_core::config::Tailscale::Auto,
        };
        let service = Arc::new(
            MailService::open(dir.path(), node, SigningKey::from_bytes(&[11u8; 32]))
                .expect("service"),
        );
        let router = router(Arc::clone(&service));
        (dir, router, service)
    }

    /// Put `friend` in the address book as a member.
    pub(super) fn admit(service: &Arc<MailService>, seed: u8) -> NodeId {
        let friend = hivemind_core::identity::Identity::from_seed([seed; 32]).expect("identity");
        let id = friend.node_id();
        service
            .admit(
                id,
                "friend",
                Some("ana"),
                friend.certificate_der().to_vec(),
                hivemind_core::peerbook::PeerAddr::manual("10.0.0.9", 8400),
            )
            .expect("admit");
        id
    }

    #[tokio::test]
    async fn the_peer_list_says_who_is_online_and_what_they_are_working_on() {
        // SPEC §5.5 and §9.1. The address book knows nothing about presence,
        // so `list_peers` is where the two are put together — and a mutation
        // replacing the whole handler with an empty list survived until this
        // existed, which is the same finding in a different shape.
        let (_dir, router, service) = app_with_service();
        let id = admit(&service, 51);

        let (status, peers) = call(&router, get("/api/v1/peers")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(peers.as_array().expect("array").len(), 1);
        assert_eq!(peers[0]["id"], id.to_string());
        assert_eq!(
            peers[0]["online"], false,
            "a peer is not online for being in peers.toml"
        );
        assert_eq!(peers[0]["sessions"], serde_json::json!([]));

        service.mark_online(
            id,
            vec![crate::peer::SessionNote {
                label: "hivemind".to_owned(),
            }],
        );

        let (_, peers) = call(&router, get("/api/v1/peers")).await;
        assert_eq!(peers[0]["online"], true);
        assert_eq!(peers[0]["sessions"], serde_json::json!(["hivemind"]));
        assert_eq!(peers[0]["paired"], true);
        assert_eq!(peers[0]["owner"], "ana");
    }

    #[tokio::test]
    async fn a_node_only_seen_is_listed_beside_the_members_and_is_never_online() {
        // "Who can I mail?" and "why can I not mail that machine?" are the
        // same question at different moments, so both are in one list.
        let (_dir, router, service) = app_with_service();
        let member = admit(&service, 52);
        let stranger = NodeId::from_certificate_der(b"a stranger");
        service.record_seen(
            stranger,
            Some("outsider".to_owned()),
            None,
            hivemind_core::peerbook::PeerAddr::manual("10.0.0.8", 8400),
        );

        let (_, peers) = call(&router, get("/api/v1/peers")).await;
        let rows = peers.as_array().expect("array");
        assert_eq!(rows.len(), 2);

        let seen = rows
            .iter()
            .find(|row| row["id"] == stranger.to_string())
            .expect("the stranger is listed");
        assert_eq!(seen["paired"], false);
        assert_eq!(seen["online"], false);
        assert!(rows.iter().any(|row| row["id"] == member.to_string()));
    }

    pub(super) async fn call(
        router: &Router,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
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

    pub(super) fn get(path: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("request")
    }

    pub(super) fn post_json(path: &str, body: &serde_json::Value) -> Request<Body> {
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
    async fn every_response_says_when_the_running_binary_was_written() {
        // #36: reinstalling leaves the daemon serving the code it loaded, and
        // the version reads `v0.1.0` either way. Remembered first, because the
        // router reads it as it is built.
        crate::freshness::remember();
        let (_dir, router, _) = app();

        // `/healthz` and not `/api/v1/me`: the point of the header is that a
        // command which never asks about the daemon still finds out.
        let response = router
            .clone()
            .oneshot(get("/healthz"))
            .await
            .expect("respond");
        let stamped = response
            .headers()
            .get(crate::freshness::BINARY_MODIFIED)
            .expect("every response carries it")
            .to_str()
            .expect("an RFC 3339 timestamp is ascii")
            .to_owned();

        let (_, body) = call(&router, get("/api/v1/me")).await;
        assert_eq!(body["binary_modified_at"], stamped, "one fact, one source");
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
    async fn an_event_stream_ends_when_the_daemon_says_it_is_closing() {
        // The mechanism behind `tests/shutdown.rs`, asserted without a
        // process: a stream that does not end keeps its connection in flight,
        // and the graceful shutdown waits on every one of those (#108).
        let (_dir, _router, service) = app_with_service();
        let stream = events(State(Arc::clone(&service)))
            .await
            .into_response()
            .into_body();

        service.close_event_streams();

        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), stream.collect()).await;
        assert!(
            ended.is_ok(),
            "the stream should have ended once the daemon said it was closing"
        );
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
}
