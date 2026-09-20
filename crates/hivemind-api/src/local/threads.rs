//! Conversations: the list of them, and one of them (SPEC §7.1).
//!
//! A conversation **is** a thread (#43). There is no second concept beside it,
//! so both endpoints here are about the same stored thing seen from two
//! distances: `/api/v1/threads` is the chat list, and
//! `/api/v1/threads/{thread_id}` is one conversation opened.
//!
//! A child of `local` because they are loopback routes like the rest, and a
//! file of their own because `local.rs` is at the size the gate allows.

use axum::Json;
use axum::extract::{Path, Query as AxumQuery, State};
use chrono::{DateTime, Utc};
use hivemind_core::index::ConversationQuery;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::{AppState, MessageSummary};
use crate::problem::{Problem, ProblemType};

/// One conversation, as the chat list shows it (SPEC §7.1).
#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationSummary {
    /// The thread every message in it carries, which is also the id of the
    /// message that opened it. Pass it to `GET /api/v1/threads/{thread_id}` to
    /// read it, or to a reply to continue it.
    pub thread_id: String,
    /// The subject it opened with. The replies are all `Re:` it.
    pub subject: String,
    /// The machines it is with — this node is left out, unless it is the only
    /// one in the conversation.
    pub participants: Vec<String>,
    /// How many messages are in it, counted once each however many boxes hold
    /// them.
    pub messages: u64,
    /// How many of those are still unread.
    pub unread: u64,
    /// The most recent message's id.
    pub last_id: String,
    /// Who sent the most recent message.
    pub last_from: String,
    /// `human` if a person typed the most recent message, `agent` if a Claude
    /// sent it.
    pub last_sender_kind: String,
    /// When it was sent. The list is ordered by this, newest first.
    pub last_at: DateTime<Utc>,
}

impl From<hivemind_core::index::Conversation> for ConversationSummary {
    fn from(conversation: hivemind_core::index::Conversation) -> Self {
        Self {
            thread_id: conversation.thread_id.to_string(),
            subject: conversation.subject,
            participants: conversation
                .participants
                .iter()
                .map(ToString::to_string)
                .collect(),
            messages: conversation.messages,
            unread: conversation.unread,
            last_id: conversation.last_id.to_string(),
            last_from: conversation.last_from.to_string(),
            last_sender_kind: conversation.last_sender_kind.as_str().to_owned(),
            last_at: conversation.last_at,
        }
    }
}

/// Which conversations to list (SPEC §7.1).
///
/// `deny_unknown_fields` for the reason #28 gives: a filter that is accepted
/// and ignored answers a narrow question with a broad answer, and nothing says
/// so.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[serde(default, deny_unknown_fields)]
pub struct ListThreadsParams {
    /// Only conversations with this machine: a node id, whole or in the short
    /// form `/api/v1/peers` shows.
    pub with: Option<String>,
    /// How many to return.
    pub limit: Option<usize>,
}

#[utoipa::path(
    get, path = "/api/v1/threads",
    params(ListThreadsParams),
    responses((status = 200, body = Vec<ConversationSummary>), (status = 400, body = Problem)),
    tag = "messages"
)]
pub(crate) async fn list_threads(
    State(service): State<AppState>,
    params: Result<AxumQuery<ListThreadsParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<ConversationSummary>>, Problem> {
    let AxumQuery(params) = params.map_err(|rejection| {
        Problem::new(
            ProblemType::InvalidQuery,
            format!("{}. Accepted: with, limit.", rejection.body_text()),
        )
    })?;

    // Resolved rather than parsed, so the short id every other door accepts
    // works here too — and a machine this node has never met is refused rather
    // than answered with an empty list, which would read as "no conversations
    // with them" (#102).
    let with = params
        .with
        .as_deref()
        .map(|typed| {
            service.resolve_peer(typed.trim()).map_err(|_| {
                Problem::new(
                    ProblemType::InvalidQuery,
                    format!(
                        "`{typed}` is not a machine this node knows; `with` takes a \
                         node id, whole or in its short form"
                    ),
                )
            })
        })
        .transpose()?;

    Ok(Json(
        service
            .conversations(&ConversationQuery {
                with,
                limit: params.limit,
            })?
            .into_iter()
            .map(ConversationSummary::from)
            .collect(),
    ))
}

#[utoipa::path(
    get, path = "/api/v1/threads/{thread_id}",
    params((
        "thread_id" = String, Path,
        description = "The thread id, or the id of any message in it, whole or the tail the inbox prints"
    )),
    responses((status = 200, body = Vec<MessageSummary>), (status = 404, body = Problem)),
    tag = "messages"
)]
pub(crate) async fn get_thread(
    State(service): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<MessageSummary>>, Problem> {
    let id = service.resolve_message(&thread_id)?;
    Ok(Json(
        service
            .thread_of(id)?
            .into_iter()
            .map(MessageSummary::from)
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::super::tests::{admit, app_with_service, call, get, post_json};

    /// Open a conversation by sending to this machine, and answer it `replies`
    /// times. Returns the thread id.
    ///
    /// Through the router rather than the service, because what is under test
    /// is the endpoint: the list has to see what the send door wrote.
    async fn conversation(
        router: &axum::Router,
        to: &str,
        subject: &str,
        replies: usize,
    ) -> String {
        let (status, accepted) = call(
            router,
            post_json(
                "/api/v1/messages",
                &serde_json::json!({ "to": [to], "subject": subject, "body": "body" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
        let thread = accepted["thread_id"]
            .as_str()
            .expect("a thread id")
            .to_owned();

        for _ in 0..replies {
            let (status, answer) = call(
                router,
                post_json(
                    &format!("/api/v1/messages/{thread}/reply"),
                    &serde_json::json!({ "body": "and another thing" }),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{answer}");
        }
        thread
    }

    #[tokio::test]
    async fn the_list_is_one_row_per_conversation_newest_first() {
        // Two conversations with the same machine, which is the whole of #43:
        // `GET /api/v1/messages` answers these as four loose rows.
        let (_dir, router, service) = app_with_service();
        let me = service.identity().to_string();
        let first = conversation(&router, &me, "dashboard PR", 2).await;
        let second = conversation(&router, &me, "lunch?", 0).await;

        let (status, listed) = call(&router, get("/api/v1/threads")).await;

        assert_eq!(status, StatusCode::OK);
        let rows = listed.as_array().expect("an array");
        assert_eq!(
            rows.len(),
            2,
            "two conversations, not four messages: {listed}"
        );
        assert_eq!(rows[0]["thread_id"], second, "the one that moved last");
        assert_eq!(rows[0]["subject"], "lunch?");
        assert_eq!(rows[1]["thread_id"], first);
        assert_eq!(rows[1]["subject"], "dashboard PR", "not `Re: dashboard PR`");
        assert_eq!(rows[1]["messages"], 3, "counted once each, not per box");
        assert_eq!(rows[1]["unread"], 3);
        assert_eq!(rows[1]["participants"], serde_json::json!([me]));
        assert_eq!(rows[1]["last_sender_kind"], "human");
    }

    #[tokio::test]
    async fn with_is_applied_rather_than_accepted_and_ignored() {
        // A machine with no conversations has none, while the unfiltered list
        // has two. A `with` that fell through would answer the narrow question
        // with the broad answer and say nothing (#102).
        let (_dir, router, service) = app_with_service();
        let me = service.identity().to_string();
        conversation(&router, &me, "dashboard PR", 0).await;
        conversation(&router, &me, "lunch?", 0).await;
        let stranger = admit(&service, 61);

        let (_, everything) = call(&router, get("/api/v1/threads")).await;
        assert_eq!(everything.as_array().expect("an array").len(), 2);

        // The short form, because that is what `/api/v1/peers` shows.
        let (status, theirs) = call(
            &router,
            get(&format!("/api/v1/threads?with={}", stranger.short())),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            theirs.as_array().expect("an array").len(),
            0,
            "we have never written to them: {theirs}"
        );
    }

    #[tokio::test]
    async fn a_with_that_names_no_machine_is_refused_rather_than_answered_empty() {
        // An empty list reads as "no conversations with them", which is a
        // wrong answer that looks like a right one (#28).
        let (_dir, router, service) = app_with_service();
        conversation(&router, &service.identity().to_string(), "dashboard PR", 0).await;

        let (status, problem) = call(&router, get("/api/v1/threads?with=nobody-here")).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(problem["type"], "/problems/invalid-query");
        let detail = problem["detail"].as_str().expect("a detail");
        assert!(detail.contains("nobody-here"), "{detail}");
    }

    #[tokio::test]
    async fn a_parameter_nobody_defined_is_refused_rather_than_ignored() {
        let (_dir, router, _service) = app_with_service();

        let (status, problem) = call(&router, get("/api/v1/threads?box=new")).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(problem["type"], "/problems/invalid-query");
        let detail = problem["detail"].as_str().expect("a detail");
        assert!(detail.contains("with"), "say what it does take: {detail}");
    }

    #[tokio::test]
    async fn the_limit_is_applied() {
        let (_dir, router, service) = app_with_service();
        let me = service.identity().to_string();
        conversation(&router, &me, "dashboard PR", 0).await;
        let newest = conversation(&router, &me, "lunch?", 0).await;

        let (_, listed) = call(&router, get("/api/v1/threads?limit=1")).await;

        let rows = listed.as_array().expect("an array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["thread_id"], newest);
    }
}
