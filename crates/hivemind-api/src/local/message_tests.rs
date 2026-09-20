//! The mail endpoints of the loopback API: listing a box and its filters,
//! sending, reading, replying, the refusals each of those owes, search and
//! paging.
//!
//! Split from `local.rs` when its tests passed the size gate, along the line
//! between the node's own surface — who am I, who can I see, is this daemon
//! current — and what the node carries.

use axum::http::StatusCode;
use hivemind_core::peer::NodeId;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use ulid::Ulid;

use super::tests::{app, app_with_service, call, get, post_json};
use crate::problem::ProblemType;

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
    // `is_string` held for `""` too, so it said nothing about whether the
    // type's title reached the body at all (#97).
    assert_eq!(
        problem["title"],
        ProblemType::MessageNotFound.title(),
        "the body carries the problem type's own title"
    );
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

/// Send a message to this node, and hand back its id.
async fn sent_to_self(router: &axum::Router, identity: &NodeId, subject: &str) -> String {
    let (_, accepted) = call(
        router,
        post_json(
            "/api/v1/messages",
            &serde_json::json!({
                "to": [identity.to_string()],
                "subject": subject,
                "body": "?",
            }),
        ),
    )
    .await;
    accepted["id"].as_str().expect("an id").to_owned()
}

#[tokio::test]
async fn a_thread_opens_on_the_id_of_any_message_in_it_and_holds_only_that_thread() {
    // Nobody knows by heart which message came first, so the id of a reply has
    // to open the conversation as well as the root's does (#34) — and the
    // short form, because that is what the inbox prints (#27).
    //
    // Two threads, because a filter that returned *everything* is exactly how
    // #28 passed its test: with one thread in the store, "the right messages"
    // and "all the messages" are the same list.
    let (_dir, router, identity) = app();
    let root = sent_to_self(&router, &identity, "dashboard PR").await;
    let elsewhere = sent_to_self(&router, &identity, "lunch").await;
    let (_, replied) = call(
        &router,
        post_json(
            &format!("/api/v1/messages/{root}/reply"),
            &serde_json::json!({ "body": "on it" }),
        ),
    )
    .await;
    let reply = replied["id"].as_str().expect("an id").to_owned();

    for opened in [root.clone(), reply.clone(), reply[20..].to_owned()] {
        let (status, thread) = call(&router, get(&format!("/api/v1/threads/{opened}"))).await;
        assert_eq!(status, StatusCode::OK, "opened by {opened}");

        let ids: Vec<&str> = thread
            .as_array()
            .expect("an array")
            .iter()
            .map(|m| m["id"].as_str().expect("an id"))
            .collect();
        // Oldest first, once each — a message addressed to its own sender is
        // indexed in two boxes — and nothing from the other conversation.
        assert_eq!(ids, [root.as_str(), reply.as_str()], "opened by {opened}");
    }

    let (_, alone) = call(&router, get(&format!("/api/v1/threads/{elsewhere}"))).await;
    assert_eq!(
        alone.as_array().expect("an array").len(),
        1,
        "the other thread is still its own"
    );
}

#[tokio::test]
async fn a_thread_nobody_has_is_a_404_rather_than_an_empty_list() {
    // An empty list reads as an empty conversation, which is the mistake #28
    // was filed for: a question that made no sense answered as though it did.
    let (_dir, router, _) = app();

    let (status, problem) = call(
        &router,
        get(&format!("/api/v1/threads/{}", Ulid::generate())),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["type"], "/problems/message-not-found");
}

#[tokio::test]
async fn a_listing_says_how_far_each_sent_message_has_got() {
    // #31: the sender used to see a global `outbox: 1` that said *something*
    // was outstanding and never what or to whom. Two recipients, neither
    // reached, so the counts are distinguishable from "one" and from "all".
    let (_dir, router, service) = app_with_service();
    let ana = super::tests::admit(&service, 61);
    let beto = super::tests::admit(&service, 62);

    call(
        &router,
        post_json(
            "/api/v1/messages",
            &serde_json::json!({
                "to": [ana.to_string(), beto.to_string()],
                "subject": "two recipients",
                "body": "x",
            }),
        ),
    )
    .await;

    let (_, list) = call(&router, get("/api/v1/messages?box=out")).await;
    let delivery = &list[0]["delivery"];
    assert_eq!(delivery["state"], "queued");
    assert_eq!(delivery["recipients"], 2);
    assert_eq!(delivery["delivered"], 0);
    assert_eq!(delivery["read"], 0);
}

#[tokio::test]
async fn received_mail_has_no_delivery_to_report() {
    // Nothing to confirm is not the same as nothing confirmed, and a mark on
    // somebody else's message would be a claim about our own machine.
    let (_dir, router, identity) = app();
    call(
        &router,
        post_json(
            "/api/v1/messages",
            &serde_json::json!({ "to": [identity.to_string()], "subject": "note to self", "body": "x" }),
        ),
    )
    .await;

    let (_, list) = call(&router, get("/api/v1/messages?box=new")).await;
    assert_eq!(list[0]["delivery"], serde_json::Value::Null);

    let (_, sent) = call(&router, get("/api/v1/messages?box=sent")).await;
    assert_eq!(
        sent[0]["delivery"],
        serde_json::Value::Null,
        "a note to self has no recipient to confirm anything"
    );
}

#[tokio::test]
async fn reading_one_message_lists_what_each_recipient_has_done() {
    // The other half of what #31 asks for: on the message itself, a line per
    // recipient — including why the ones that have not had it have not.
    let (_dir, router, service) = app_with_service();
    let ana = super::tests::admit(&service, 63);
    let beto = super::tests::admit(&service, 64);

    let (_, accepted) = call(
        &router,
        post_json(
            "/api/v1/messages",
            &serde_json::json!({
                "to": [ana.to_string(), beto.to_string()],
                "subject": "two recipients",
                "body": "x",
            }),
        ),
    )
    .await;
    let id: Ulid = accepted["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("ulid");

    // Ana's machine took it and she read it; Beto's is off.
    let mut outbound = service
        .delivery_of(id)
        .expect("ours")
        .expect("we sent it")
        .clone();
    let now = chrono::Utc::now();
    for state in &mut outbound {
        if state.node == ana {
            state.delivered_at = Some(now);
            state.read_at = Some(now);
        } else {
            state.attempts = 3;
            state.last_error = Some("could not reach 10.0.0.9:8400".to_owned());
        }
    }
    service
        .save_outbound(&hivemind_core::store::Outbound {
            message: service.get(id).expect("get").1,
            recipients: outbound,
        })
        .expect("save");

    let (_, message) = call(&router, get(&format!("/api/v1/messages/{id}"))).await;
    let lines = message["recipients"].as_array().expect("a line each");
    assert_eq!(lines.len(), 2);

    let hers = lines
        .iter()
        .find(|line| line["node"] == ana.to_string())
        .expect("ana");
    assert_eq!(hers["state"], "read");
    assert!(hers["read_at"].is_string());

    let his = lines
        .iter()
        .find(|line| line["node"] == beto.to_string())
        .expect("beto");
    assert_eq!(his["state"], "queued");
    assert_eq!(his["attempts"], 3);
    assert_eq!(his["last_error"], "could not reach 10.0.0.9:8400");
}
