//! The mail endpoints of the loopback API: listing a box and its filters,
//! sending, reading, replying, the refusals each of those owes, search and
//! paging.
//!
//! Split from `local.rs` when its tests passed the size gate, along the line
//! between the node's own surface — who am I, who can I see, is this daemon
//! current — and what the node carries.

use axum::http::StatusCode;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use ulid::Ulid;

use super::tests::{app, app_with_service, call, get, post_json};

#[tokio::test]
async fn a_mailbox_that_is_not_one_is_refused_rather_than_ignored() {
    // #28. `?box=banana` used to return the whole list with a 200: the
    // comment said it filtered to nothing, and `mailbox: None` means
    // "every mailbox". A wrong answer that looks like a right one.
    let (_dir, router, _service) = app_with_service();

    let (status, problem) = call(&router, get("/api/v1/messages?box=banana")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], "/problems/invalid-query");
    assert!(
        problem["detail"]
            .as_str()
            .expect("a detail")
            .contains("sent"),
        "it should say what a mailbox is: {problem}"
    );
}

#[tokio::test]
async fn a_parameter_nobody_defined_is_refused_rather_than_ignored() {
    // How this was found: the Claude on the Arch machine wrote
    // `mailbox=` instead of `box=`, watched two different questions give
    // the same answer, and concluded the `out` mailbox was broken. It was
    // not. What was broken was the API not saying the question made no
    // sense (#28).
    let (_dir, router, _service) = app_with_service();

    let (status, problem) = call(&router, get("/api/v1/messages?mailbox=sent")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], "/problems/invalid-query");
    let detail = problem["detail"].as_str().expect("a detail");
    assert!(detail.contains("mailbox"), "name what was wrong: {detail}");
    assert!(detail.contains("box"), "and what was meant: {detail}");
}

#[tokio::test]
async fn the_filters_that_do_exist_still_work() {
    // So the two tests above cannot pass by every query being refused.
    let (_dir, router, _service) = app_with_service();

    for query in [
        "/api/v1/messages",
        "/api/v1/messages?box=sent",
        "/api/v1/messages?unread=true&limit=5",
        "/api/v1/messages?q=anything",
    ] {
        let (status, _) = call(&router, get(query)).await;
        assert_eq!(status, StatusCode::OK, "{query}");
    }
}

#[tokio::test]
async fn a_stale_cursor_is_still_forgiven() {
    // Deliberately unlike the others: a cursor this version did not issue
    // comes from a page somebody left open, not from a caller who got it
    // wrong. It filters to nothing rather than erroring.
    let (_dir, router, _service) = app_with_service();

    let (status, _) = call(&router, get("/api/v1/messages?cursor=nonsense")).await;
    assert_eq!(status, StatusCode::OK);
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
async fn sending_the_same_thing_twice_says_which_one_it_repeats() {
    // #33: the send is still accepted — the second id comes back and the
    // message goes — and the notice is for whoever pressed twice.
    let (_dir, router, identity) = app();
    let twice = || {
        post_json(
            "/api/v1/messages",
            &serde_json::json!({
                "to": [identity.to_string()],
                "subject": "primeiro contato",
                "body": "olá",
            }),
        )
    };

    let (_, first) = call(&router, twice()).await;
    assert_eq!(first["duplicate_of"], serde_json::Value::Null);

    let (status, second) = call(&router, twice()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(second["duplicate_of"], first["id"]);
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
async fn an_unknown_mailbox_filter_is_refused_with_a_mailbox_in_the_store() {
    // This test used to assert the opposite — "returns nothing rather
    // than failing" — and it passed for the wrong reason: `app()` has no
    // messages, so an empty list is what comes back whether the filter
    // works or not. The code in fact returned *everything*, because
    // `mailbox: None` means "every mailbox", and nobody found out until
    // somebody ran it against a real inbox (#28).
    //
    // So this one sends a message first. With nothing in the store there
    // is nothing this endpoint can get wrong.
    let (_dir, router, identity) = app();
    call(
        &router,
        post_json(
            "/api/v1/messages",
            &serde_json::json!({
                "to": [identity.to_string()],
                "subject": "something to find",
                "body": "x",
            }),
        ),
    )
    .await;

    let (status, problem) = call(&router, get("/api/v1/messages?box=nonsense")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], "/problems/invalid-query");

    // And the real filters still answer, so the refusal above is about
    // the value rather than about the parameter existing.
    let (status, body) = call(&router, get("/api/v1/messages?box=sent")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().expect("array").len(), 1);
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
