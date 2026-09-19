//! The web UI (SPEC §11).
//!
//! Server-rendered with `askama`, and **reading works with JavaScript
//! switched off**. That is not a box-tick: it is what makes the UI usable when
//! the daemon is up and something in the page is broken, which is exactly when
//! somebody needs to read their mail.
//!
//! So every action here is a form that posts and redirects. The TypeScript in
//! `assets/` makes the inbox update live and lets files be dropped on the
//! compose box; nothing depends on it.

// Both sides of these handlers are a `Response`: a failure here is a page a
// person reads, not a document a client parses. Boxing the error variant to
// make it smaller than the success variant would be strictly worse.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query as AxumQuery, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use hivemind_core::index::Query;
use hivemind_core::message::{Kind, Recipient, SenderKind};
use hivemind_core::store::Mailbox;
use serde::Deserialize;

use crate::service::{Draft, MailService};

/// Everything the layout needs, on every page.
struct Chrome {
    here: &'static str,
    unread: u64,
    node_id: String,
    short_id: String,
}

impl Chrome {
    fn new(service: &MailService, here: &'static str) -> Self {
        let id = service.identity();
        Self {
            here,
            // A failure here is not worth a 500 on a page that would otherwise
            // render: the badge is missing, the mail is not.
            unread: service.unread_count().unwrap_or(0),
            node_id: id.to_string(),
            short_id: id.short(),
        }
    }
}

/// One row of a listing.
struct Row {
    id: String,
    thread_id: String,
    from: String,
    from_short: String,
    subject: String,
    kind: String,
    sender_kind: String,
    sent_at: String,
    when: String,
    unread: bool,
    attachment_names: Vec<String>,
}

/// One message in a thread.
struct Post {
    id: String,
    from: String,
    from_short: String,
    sender_kind: String,
    sent_at: String,
    when: String,
    body: String,
    attachments: Vec<Attached>,
}

/// One attachment, as the page shows it.
struct Attached {
    name: String,
    sha256: String,
    human_size: String,
    cached: bool,
}

/// One peer, a member or merely seen.
struct PeerRow {
    id: String,
    short_id: String,
    name: String,
    owner: Option<String>,
    addr: String,
    last_seen: Option<String>,
    /// Whether it said hello within the last two intervals (SPEC §5.5).
    online: bool,
    /// What each Claude Code session on that machine is working on.
    sessions: Vec<String>,
}

#[derive(Template)]
#[template(path = "inbox.html")]
struct InboxPage {
    here: &'static str,
    unread: u64,
    node_id: String,
    short_id: String,
    heading: &'static str,
    path: &'static str,
    empty: &'static str,
    live: bool,
    query: String,
    unread_only: bool,
    messages: Vec<Row>,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadPage {
    here: &'static str,
    unread: u64,
    node_id: String,
    short_id: String,
    thread_id: String,
    subject: String,
    messages: Vec<Post>,
}

#[derive(Template)]
#[template(path = "compose.html")]
struct ComposePage {
    here: &'static str,
    unread: u64,
    node_id: String,
    short_id: String,
    problem: Option<String>,
    to: String,
    subject: String,
    body: String,
    peers: Vec<PeerRow>,
}

#[derive(Template)]
#[template(path = "peers.html")]
struct PeersPage {
    here: &'static str,
    unread: u64,
    node_id: String,
    short_id: String,
    problem: Option<String>,
    in_group: bool,
    paired: Vec<PeerRow>,
    seen: Vec<PeerRow>,
}

/// Build the web UI's routes.
pub fn router(state: Arc<MailService>) -> Router {
    Router::new()
        .route("/", get(inbox))
        .route("/sent", get(sent))
        .route("/thread/{id}", get(thread))
        .route("/thread/{id}/reply", post(reply))
        .route("/compose", get(compose).post(send))
        .route("/peers", get(peers))
        .route("/peers/join", post(join))
        .route("/peers/{id}/remove", post(forget))
        .route("/peers/refresh", post(refresh))
        .route("/assets/{file}", get(asset))
        .with_state(state)
}

/// What a listing page was asked for.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// Free text to match against subject and body.
    #[serde(default)]
    pub q: Option<String>,
    /// Show only unread mail.
    #[serde(default)]
    pub unread: Option<bool>,
}

/// The inbox: everything received, newest first.
async fn inbox(
    State(service): State<Arc<MailService>>,
    AxumQuery(params): AxumQuery<ListQuery>,
) -> Result<Response, Response> {
    listing(
        &service,
        "inbox",
        "Inbox",
        "/",
        "Nothing here yet.",
        true,
        &params,
        &[Mailbox::New, Mailbox::Cur],
    )
}

/// Sent mail, including what is still on its way.
async fn sent(
    State(service): State<Arc<MailService>>,
    AxumQuery(params): AxumQuery<ListQuery>,
) -> Result<Response, Response> {
    listing(
        &service,
        "sent",
        "Sent",
        "/sent",
        "You have not sent anything yet.",
        false,
        &params,
        // `out/` as well as `sent/`: a message still being delivered is one
        // you sent, and hiding it until the last recipient takes it would make
        // a peer being offline look like the message vanishing.
        &[Mailbox::Out, Mailbox::Sent],
    )
}

#[allow(clippy::too_many_arguments)]
fn listing(
    service: &Arc<MailService>,
    here: &'static str,
    heading: &'static str,
    path: &'static str,
    empty: &'static str,
    live: bool,
    params: &ListQuery,
    mailboxes: &[Mailbox],
) -> Result<Response, Response> {
    let chrome = Chrome::new(service, here);
    let unread_only = params.unread.unwrap_or(false);
    let text = params.q.clone().filter(|q| !q.trim().is_empty());

    // One query per mailbox, because the index filters by one at a time and a
    // message addressed to its own sender genuinely exists in two of them —
    // once as something received and once as something sent.
    let mut messages: Vec<Row> = Vec::new();
    for mailbox in mailboxes {
        let query = Query {
            mailbox: Some(*mailbox),
            thread: None,
            from: None,
            unread_only,
            text: text.clone(),
            limit: Some(200),
            // The page is read in one go: it renders a list somebody scrolls,
            // not an API a client walks. Paging belongs here the day 200
            // messages stops being enough to look at.
            cursor: None,
        };
        messages.extend(
            service
                .list(&query)
                .map_err(render_error)?
                .into_iter()
                .map(Row::from),
        );
    }
    // Newest first across both, then capped again: two full pages merged is
    // twice a page.
    messages.sort_by(|a, b| b.sent_at.cmp(&a.sent_at));
    messages.truncate(200);

    Ok(page(InboxPage {
        here: chrome.here,
        unread: chrome.unread,
        node_id: chrome.node_id,
        short_id: chrome.short_id,
        heading,
        path,
        empty,
        live,
        query: text.unwrap_or_default(),
        unread_only,
        messages,
    }))
}

/// One conversation, oldest first, marking everything in it read.
async fn thread(
    State(service): State<Arc<MailService>>,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let chrome = Chrome::new(&service, "inbox");
    let thread_id: ulid::Ulid = id.parse().map_err(|_| not_found("no such thread"))?;

    let summaries = service.thread(thread_id).map_err(render_error)?;
    if summaries.is_empty() {
        return Err(not_found("no such thread"));
    }

    let subject = summaries
        .first()
        .map_or_else(String::new, |s| s.subject.clone());

    let mut messages = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let (_, message) = service.get(summary.id).map_err(render_error)?;
        // Opening a conversation is reading it. The API keeps the two separate
        // so a preview can avoid this; a page that renders the body cannot.
        let _ = service.mark_read(summary.id);
        messages.push(Post::from_message(&service, message));
    }

    Ok(page(ThreadPage {
        here: chrome.here,
        // Counted before the messages above were marked read, so recount.
        unread: service.unread_count().unwrap_or(0),
        node_id: chrome.node_id,
        short_id: chrome.short_id,
        thread_id: thread_id.to_string(),
        subject,
        messages,
    }))
}

/// What a reply form sends.
#[derive(Debug, Deserialize)]
pub struct ReplyForm {
    /// The reply body.
    pub body: String,
}

async fn reply(
    State(service): State<Arc<MailService>>,
    Path(id): Path<String>,
    axum::Form(form): axum::Form<ReplyForm>,
) -> Result<Redirect, Response> {
    let thread_id: ulid::Ulid = id.parse().map_err(|_| not_found("no such thread"))?;
    // Reply to the last message in the thread: that is what the box under it
    // means, and replying to the root would lose the context of a long one.
    let last = service
        .thread(thread_id)
        .map_err(render_error)?
        .last()
        .map(|s| s.id)
        .ok_or_else(|| not_found("no such thread"))?;

    // A person typed this into a browser (SPEC §4.1).
    service
        .reply(last, form.body, Vec::new(), SenderKind::Human)
        .map_err(render_error)?;

    Ok(Redirect::to(&format!("/thread/{thread_id}")))
}

async fn compose(State(service): State<Arc<MailService>>) -> Response {
    let chrome = Chrome::new(&service, "compose");
    page(ComposePage {
        here: chrome.here,
        unread: chrome.unread,
        node_id: chrome.node_id,
        short_id: chrome.short_id,
        problem: None,
        to: String::new(),
        subject: String::new(),
        body: String::new(),
        peers: peer_rows(&service),
    })
}

/// Send from the compose form.
///
/// Multipart, because the form carries files. A browser with no JavaScript
/// posts exactly this.
async fn send(
    State(service): State<Arc<MailService>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, Response> {
    let mut to = String::new();
    let mut subject = String::new();
    let mut body = String::new();
    // Files arrive as bytes and the service takes paths, so they are staged
    // here first. The directory is dropped once the send returns, by which
    // time the blob store holds its own copies.
    let staging = tempdir().map_err(render_error)?;
    let mut files = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(|e| bad_request(&e))? {
        let name = field.name().unwrap_or_default().to_owned();
        let filename = field.file_name().map(ToOwned::to_owned);

        match name.as_str() {
            "to" => to = field.text().await.map_err(|e| bad_request(&e))?,
            "subject" => subject = field.text().await.map_err(|e| bad_request(&e))?,
            "body" => body = field.text().await.map_err(|e| bad_request(&e))?,
            "files" => {
                // An empty file input still sends one part, with no name.
                let Some(filename) = filename.filter(|f| !f.is_empty()) else {
                    continue;
                };
                let bytes = field.bytes().await.map_err(|e| bad_request(&e))?;
                if bytes.is_empty() {
                    continue;
                }
                // The browser's name is checked before it becomes a path,
                // because this one came from whatever the person dropped in.
                hivemind_core::blobs::check_attachment_name(&filename)
                    .map_err(|e| render_error(e.into()))?;
                let path = staging.path().join(&filename);
                std::fs::write(&path, &bytes).map_err(|e| {
                    render_error(
                        hivemind_core::blobs::BlobError::Io {
                            context: format!("could not stage {filename}"),
                            source: e,
                        }
                        .into(),
                    )
                })?;
                files.push(path);
            }
            _ => {}
        }
    }

    // Through the service, which knows the address book: a short id is a peer
    // and not a person's name (#19).
    let recipients: Vec<Recipient> = match to
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|typed| service.parse_recipient(typed))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(recipients) => recipients,
        Err(error) => return Ok(render_error(error)),
    };

    let draft = Draft {
        to: recipients,
        subject: subject.clone(),
        body: body.clone(),
        kind: Kind::Message,
        in_reply_to: None,
        attachments: files,
    };

    // A person typed this into a browser (SPEC §4.1).
    match service.send(draft, SenderKind::Human) {
        Ok(message) => Ok(Redirect::to(&format!("/thread/{}", message.thread_id)).into_response()),
        Err(error) => {
            // Re-render with what they wrote still in the boxes. Losing a
            // half-written message to a typo in the recipient would be its own
            // small tragedy.
            let chrome = Chrome::new(&service, "compose");
            Ok(page(ComposePage {
                here: chrome.here,
                unread: chrome.unread,
                node_id: chrome.node_id,
                short_id: chrome.short_id,
                problem: Some(error.to_string()),
                to,
                subject,
                body,
                peers: peer_rows(&service),
            }))
        }
    }
}

async fn peers(State(service): State<Arc<MailService>>) -> Response {
    peers_page(&service, None)
}

/// The join-by-address form.
#[derive(Debug, Deserialize)]
struct JoinForm {
    host: String,
}

async fn join(
    State(service): State<Arc<MailService>>,
    axum::Form(form): axum::Form<JoinForm>,
) -> Response {
    match service.join(form.host.trim()).await {
        Ok(crate::service::Met::Member(_)) => Redirect::to("/peers").into_response(),
        // Reached, but not a member. Saying so beats a redirect to a list
        // that looks as though nothing happened.
        Ok(crate::service::Met::Stranger(node)) => peers_page(
            &service,
            Some(format!(
                "{} answered, but it is not in this node's group",
                node.name.unwrap_or_else(|| form.host.clone())
            )),
        ),
        Err(error) => peers_page(&service, Some(error.to_string())),
    }
}

async fn forget(
    State(service): State<Arc<MailService>>,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let node = service.resolve_peer(&id).map_err(render_error)?;
    match service.remove_peer(node) {
        Ok(()) => Ok(Redirect::to("/peers").into_response()),
        Err(error) => Ok(peers_page(&service, Some(error.to_string()))),
    }
}

async fn refresh(State(service): State<Arc<MailService>>) -> Response {
    match service.refresh_peers().await {
        Ok(_) => Redirect::to("/peers").into_response(),
        Err(error) => peers_page(&service, Some(error.to_string())),
    }
}

fn peers_page(service: &Arc<MailService>, problem: Option<String>) -> Response {
    let chrome = Chrome::new(service, "peers");
    page(PeersPage {
        here: chrome.here,
        unread: chrome.unread,
        node_id: chrome.node_id,
        short_id: chrome.short_id,
        problem,
        in_group: service.in_group().unwrap_or(false),
        paired: peer_rows(service),
        seen: service
            .seen_nodes()
            .unwrap_or_default()
            .into_iter()
            .map(|s| PeerRow {
                id: s.id.to_string(),
                short_id: s.id.short(),
                name: s.name.unwrap_or_else(|| s.addr.host.clone()),
                owner: s.owner,
                addr: s.addr.authority(),
                // A node outside the group never says hello.
                online: false,
                sessions: Vec::new(),
                last_seen: Some(relative(s.last_seen)),
            })
            .collect(),
    })
}

fn peer_rows(service: &Arc<MailService>) -> Vec<PeerRow> {
    service
        .paired_peers()
        .unwrap_or_default()
        .into_iter()
        .map(|peer| {
            let presence = service.presence_of(peer.id);
            PeerRow {
                id: peer.id.to_string(),
                short_id: peer.id.short(),
                addr: peer
                    .addrs_by_preference()
                    .first()
                    .map_or_else(|| "no known address".to_owned(), |a| a.authority()),
                last_seen: peer.last_seen.map(relative),
                online: presence.is_some(),
                sessions: presence
                    .map(|p| p.sessions.into_iter().map(|s| s.label).collect())
                    .unwrap_or_default(),
                name: peer.name,
                owner: peer.owner,
            }
        })
        .collect()
}

/// The assets the binary carries (SPEC §11).
///
/// Embedded rather than read from disk: a single binary that serves its own UI
/// has nothing to install wrongly and nothing to get out of step with it.
static ASSETS: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/assets");

async fn asset(Path(file): Path<String>) -> Response {
    // Three things already make traversal impossible here: the route matches
    // one path segment, `include_dir` looks up by exact name, and there is no
    // filesystem access at all. This check is redundant today and stays anyway
    // — the other three are properties of libraries and routing, and a change
    // to either would remove them silently.
    let Some(entry) = ASSETS.get_file(&file).filter(|_| !file.contains('/')) else {
        return not_found("no such asset");
    };

    let content_type = match file.rsplit_once('.').map(|(_, ext)| ext) {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            // Assets change only when the binary does, and a stale stylesheet
            // against new markup looks like a bug in the UI.
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        entry.contents(),
    )
        .into_response()
}

/// Render a template, or say plainly that rendering failed.
fn page<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::error!(%error, "could not render a page");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>hivemind could not render this page</h1>".to_owned()),
            )
                .into_response()
        }
    }
}

/// A service error as a page rather than as problem+json.
///
/// Somebody reading this is in a browser, not parsing a document.
fn render_error(error: crate::service::ServiceError) -> Response {
    let problem = crate::problem::Problem::from(error);
    let status = StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Html(format!(
            "<h1>{}</h1><p>{}</p><p><a href=\"/\">Back to the inbox</a></p>",
            escape(&problem.title),
            escape(&problem.detail)
        )),
    )
        .into_response()
}

fn not_found(detail: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(format!(
            "<h1>Not found</h1><p>{}</p><p><a href=\"/\">Back to the inbox</a></p>",
            escape(detail)
        )),
    )
        .into_response()
}

fn bad_request(error: &axum::extract::multipart::MultipartError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Html(format!(
            "<h1>That form could not be read</h1><p>{}</p>",
            escape(&error.to_string())
        )),
    )
        .into_response()
}

/// Escape the five characters that matter, for the handful of places that
/// build HTML without a template.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// A temporary directory for staged uploads.
fn tempdir() -> Result<tempfile::TempDir, crate::service::ServiceError> {
    tempfile::tempdir().map_err(|source| {
        hivemind_core::blobs::BlobError::Io {
            context: "could not make a place to stage uploads".to_owned(),
            source,
        }
        .into()
    })
}

/// "3 minutes ago", for a reader rather than a log.
fn relative(when: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = chrono::Utc::now().signed_duration_since(when).num_seconds();
    match seconds {
        // A negative interval means a clock ran backwards, which is ordinary
        // on a laptop that slept. It is not "-3 minutes ago".
        ..=59 => "just now".to_owned(),
        60..=3599 => plural(seconds / 60, "minute"),
        3600..=86_399 => plural(seconds / 3600, "hour"),
        _ => plural(seconds / 86_400, "day"),
    }
}

fn plural(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{count} {unit}s ago")
    }
}

/// A size a person can read at a glance.
fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            #[allow(clippy::cast_precision_loss)]
            let value = bytes as f64 / scale as f64;
            return format!("{value:.1} {unit}");
        }
    }
    format!("{bytes} B")
}

impl From<hivemind_core::index::Summary> for Row {
    fn from(summary: hivemind_core::index::Summary) -> Self {
        Self {
            id: summary.id.to_string(),
            thread_id: summary.thread_id.to_string(),
            from: summary.from.to_string(),
            from_short: summary.from.short(),
            subject: summary.subject,
            kind: summary.kind.as_str().to_owned(),
            sender_kind: summary.sender_kind.as_str().to_owned(),
            sent_at: summary.sent_at.to_rfc3339(),
            when: relative(summary.sent_at),
            unread: summary.mailbox == Mailbox::New,
            attachment_names: summary.attachment_names,
        }
    }
}

impl Post {
    fn from_message(service: &Arc<MailService>, message: hivemind_core::message::Message) -> Self {
        Self {
            id: message.id.to_string(),
            from: message.from.to_string(),
            from_short: message.from.short(),
            sender_kind: message.sender_kind.as_str().to_owned(),
            sent_at: message.sent_at.to_rfc3339(),
            when: relative(message.sent_at),
            body: message.body,
            attachments: message
                .attachments
                .into_iter()
                .map(|a| Attached {
                    cached: service.blobs().has(&a.sha256),
                    sha256: a.sha256.to_hex(),
                    human_size: human_size(a.size),
                    name: a.name,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use hivemind_core::crypto::SigningKey;
    use hivemind_core::peer::NodeId;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use crate::service::NodeDescription;

    fn app() -> (tempfile::TempDir, Router, Arc<MailService>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let identity = NodeId::from_certificate_der(b"this node");
        let node = NodeDescription {
            id: identity,
            certificate: b"this node".to_vec(),
            private_key: Vec::new(),
            name: "test".to_owned(),
            owner: Some("tester".to_owned()),
            callback_host: "127.0.0.1".to_owned(),
            peer_port: 8400,
            max_attachment_bytes: hivemind_core::config::DEFAULT_MAX_ATTACHMENT_BYTES,
            inline_max_bytes: hivemind_core::config::DEFAULT_INLINE_MAX_BYTES,
            prefetch: false,
            presence_interval: hivemind_core::config::DEFAULT_PRESENCE_INTERVAL,
        };
        let service = Arc::new(
            MailService::open(dir.path(), node, SigningKey::from_bytes(&[11u8; 32]))
                .expect("service"),
        );
        (dir, router(Arc::clone(&service)), service)
    }

    async fn get(router: &Router, path: &str) -> (StatusCode, String) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn post_form(router: &Router, path: &str, form: &str) -> (StatusCode, Option<String>) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form.to_owned()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        (status, location)
    }

    fn send_to_self(service: &Arc<MailService>, subject: &str, body: &str) -> ulid::Ulid {
        service
            .send(
                Draft {
                    to: vec![Recipient::Node(service.identity())],
                    subject: subject.to_owned(),
                    body: body.to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Human,
            )
            .expect("send")
            .id
    }

    #[tokio::test]
    async fn the_inbox_reads_without_any_javascript() {
        // SPEC §11. Not a box-tick: this is what makes the UI usable when the
        // daemon is up and something in the page is broken.
        let (_dir, router, service) = app();
        send_to_self(&service, "a subject", "a body");

        let (status, html) = get(&router, "/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("a subject"), "the mail should be in the HTML");
        // The one <script> is deferred and optional; nothing above depends on
        // it. If the list ever moves into JS, this fails.
        let without_scripts = html
            .split("<script")
            .next()
            .expect("there is always a first part");
        assert!(
            without_scripts.contains("a subject"),
            "the list must be rendered before any script tag"
        );
    }

    #[tokio::test]
    async fn every_page_renders() {
        let (_dir, router, service) = app();
        let id = send_to_self(&service, "rendered", "body");
        let (_, message) = service.get(id).expect("get");

        for path in [
            "/",
            "/sent",
            "/compose",
            "/peers",
            &format!("/thread/{}", message.thread_id),
        ] {
            let (status, html) = get(&router, path).await;
            assert_eq!(status, StatusCode::OK, "{path} should render");
            assert!(html.contains("<main id=\"main\">"), "{path} needs a main");
            assert!(html.contains("lang=\"en\""), "{path} needs a language");
        }
    }

    #[tokio::test]
    async fn the_peers_page_says_what_to_do_when_this_node_is_in_no_group() {
        // Without a group this machine can reach nobody, which is the first
        // thing somebody opening the page needs to know (ADR 0013).
        let (_dir, router, service) = app();
        let (_, html) = get(&router, "/peers").await;
        assert!(html.contains("Not in a group yet"), "{html}");
        assert!(html.contains("hivemind group create"));

        service.create_group(false).expect("create");
        let (_, html) = get(&router, "/peers").await;
        assert!(!html.contains("Not in a group yet"));
    }

    #[tokio::test]
    async fn a_node_seen_outside_the_group_is_listed_apart_from_members() {
        let (_dir, router, service) = app();
        service.record_seen(
            NodeId::from_certificate_der(b"somebody else"),
            Some("their-laptop".to_owned()),
            None,
            hivemind_core::peerbook::PeerAddr::manual("10.0.0.9", 8400),
        );

        let (_, html) = get(&router, "/peers").await;
        let seen = html
            .split("Seen, not in the group")
            .nth(1)
            .expect("a section for nodes only seen");
        assert!(seen.contains("their-laptop"), "{html}");
    }

    #[tokio::test]
    async fn contacting_an_address_that_does_not_answer_stays_on_the_page_and_says_so() {
        // A redirect to the list would look as though it had worked.
        let (_dir, router, _service) = app();
        let (status, location) = post_form(&router, "/peers/join", "host=127.0.0.1%3A1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(location, None, "no redirect when nothing was reached");
    }

    #[tokio::test]
    async fn a_sender_kind_shows_as_a_badge() {
        // SPEC §11: "human" or "agent", so a reader can tell at a glance
        // whether a person typed it.
        let (_dir, router, service) = app();
        service
            .send(
                Draft {
                    to: vec![Recipient::Node(service.identity())],
                    subject: "from a claude".to_owned(),
                    body: "x".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Agent,
            )
            .expect("send");

        let (_, html) = get(&router, "/").await;
        assert!(
            html.contains("<span class=\"badge agent\">agent</span>"),
            "the badge should be in the markup: {html}"
        );
    }

    #[tokio::test]
    async fn a_reply_posted_from_a_form_redirects_back_to_the_thread() {
        // No JavaScript means post-and-redirect, so reloading the page does
        // not send the reply twice.
        let (_dir, router, service) = app();
        let id = send_to_self(&service, "question", "what time?");
        let (_, message) = service.get(id).expect("get");

        let (status, location) = post_form(
            &router,
            &format!("/thread/{}/reply", message.thread_id),
            "body=one+o%27clock",
        )
        .await;

        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            location.as_deref(),
            Some(format!("/thread/{}", message.thread_id).as_str())
        );

        let (_, html) = get(&router, &format!("/thread/{}", message.thread_id)).await;
        assert!(
            html.contains("one o") && html.contains("clock"),
            "the reply should be there: {html}"
        );
    }

    #[tokio::test]
    async fn opening_a_thread_marks_it_read() {
        let (_dir, router, service) = app();
        let id = send_to_self(&service, "unread", "body");
        let (_, message) = service.get(id).expect("get");
        assert_eq!(service.unread_count().expect("count"), 1);

        get(&router, &format!("/thread/{}", message.thread_id)).await;

        assert_eq!(
            service.unread_count().expect("count"),
            0,
            "a page that renders the body has been read"
        );
    }

    #[tokio::test]
    async fn a_subject_cannot_smuggle_markup_into_the_page() {
        // The subject came from another machine.
        let (_dir, router, service) = app();
        send_to_self(&service, "<script>alert(1)</script>", "body");

        let (_, html) = get(&router, "/").await;

        assert!(
            !html.contains("<script>alert(1)</script>"),
            "askama should have escaped it"
        );
        assert!(
            html.contains("&#60;script&#62;"),
            "and it should still be readable: {html}"
        );
    }

    #[tokio::test]
    async fn a_body_cannot_smuggle_markup_into_a_thread() {
        let (_dir, router, service) = app();
        let id = send_to_self(&service, "innocent", "<img src=x onerror=alert(1)>");
        let (_, message) = service.get(id).expect("get");

        let (_, html) = get(&router, &format!("/thread/{}", message.thread_id)).await;

        assert!(!html.contains("<img src=x"), "escaped: {html}");
        assert!(
            html.contains("&#60;img src=x"),
            "and still readable: {html}"
        );
    }

    #[tokio::test]
    async fn the_stylesheet_is_served_from_the_binary() {
        let (_dir, router, _) = app();
        let (status, css) = get(&router, "/assets/hivemind.css").await;

        assert_eq!(status, StatusCode::OK);
        assert!(css.contains("prefers-color-scheme"), "SPEC §11 asks for it");
        assert!(css.contains(":focus-visible"), "and keyboard navigation");
    }

    #[tokio::test]
    async fn an_asset_path_cannot_escape_the_bundle() {
        let (_dir, router, _) = app();
        for attempt in [
            "/assets/..%2f..%2fpeers.toml",
            "/assets/../../peers.toml",
            "/assets/nothing.css",
        ] {
            let (status, _) = get(&router, attempt).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{attempt} should be refused");
        }
    }

    #[tokio::test]
    async fn a_thread_that_does_not_exist_is_a_404_not_a_500() {
        let (_dir, router, _) = app();
        for path in ["/thread/not-a-ulid", "/thread/01JXT2ZZZZZZZZZZZZZZZZZZZZ"] {
            let (status, _) = get(&router, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[test]
    fn a_time_is_shown_the_way_a_person_would_say_it() {
        let ago = |seconds: i64| relative(chrono::Utc::now() - chrono::Duration::seconds(seconds));
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(3 * 60), "3 minutes ago");
        assert_eq!(ago(3600), "1 hour ago");
        assert_eq!(ago(90_000), "1 day ago");
        // A clock that ran backwards is not "-3 minutes ago".
        assert_eq!(
            relative(chrono::Utc::now() + chrono::Duration::hours(1)),
            "just now"
        );
    }
}
