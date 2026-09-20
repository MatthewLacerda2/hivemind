//! `POST /peer/v1/receipts` (SPEC §7.2, ADR 0016).
//!
//! The endpoint is thin and its two rules are the whole of it: only a member
//! may speak, and what it says is only ever about the caller's own reading.

use axum::http::StatusCode;
use hivemind_core::message::{Kind, Recipient, SenderKind};

use super::tests::{call, identity, pair_with, service};
use crate::service::Draft;

/// One message from this node to `to`, in the outbox.
fn send_to(
    service: &std::sync::Arc<crate::service::MailService>,
    to: hivemind_core::peer::NodeId,
) -> ulid::Ulid {
    service
        .send(
            Draft {
                to: vec![Recipient::Node(to)],
                subject: "read this".to_owned(),
                body: "body".to_owned(),
                kind: Kind::Message,
                in_reply_to: None,
                attachments: Vec::new(),
            },
            SenderKind::Human,
        )
        .expect("send")
        .message
        .id
}

#[tokio::test]
async fn a_member_saying_it_read_something_is_recorded() {
    let me = identity(90);
    let (_dir, service) = service(&me);
    let ana = identity(91);
    pair_with(&service, &me, &ana).await;
    let id = send_to(&service, ana.node_id());

    let (status, body) = call(
        &service,
        &ana,
        "/peer/v1/receipts",
        &serde_json::json!({
            "read": [{ "id": id.to_string(), "read_at": "2026-09-20T10:14:00Z" }]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["recorded"], 1);
    let recipients = service.delivery_of(id).expect("ours").expect("we sent it");
    assert_eq!(recipients[0].state(), hivemind_core::store::Delivery::Read);
    assert_eq!(
        recipients[0].read_at.map(|at| at.to_rfc3339()),
        Some("2026-09-20T10:14:00+00:00".to_owned()),
        "the time is the reader's, as `sent_at` is the sender's"
    );
}

#[tokio::test]
async fn a_stranger_is_refused_rather_than_recorded() {
    // The same boundary delivery has: a node that has not proved the group key
    // may not touch the mailbox, and a receipt writes to it.
    let me = identity(92);
    let (_dir, service) = service(&me);
    let ana = identity(93);
    pair_with(&service, &me, &ana).await;
    let id = send_to(&service, ana.node_id());
    let stranger = identity(94);

    let (status, problem) = call(
        &service,
        &stranger,
        "/peer/v1/receipts",
        &serde_json::json!({
            "read": [{ "id": id.to_string(), "read_at": "2026-09-20T10:14:00Z" }]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(problem["type"], "/problems/not-paired");
    let recipients = service.delivery_of(id).expect("ours").expect("we sent it");
    assert_eq!(
        recipients[0].state(),
        hivemind_core::store::Delivery::Queued,
        "nothing a stranger says reaches the mailbox"
    );
}

#[tokio::test]
async fn a_member_cannot_report_on_somebody_elses_reading() {
    // Two members, and the message is for only one of them. The caller is the
    // connection rather than anything in the body, so the worst the other can
    // do is name a message that is not theirs — and that must change nothing.
    let me = identity(95);
    let (_dir, service) = service(&me);
    let ana = identity(96);
    let beto = identity(97);
    pair_with(&service, &me, &ana).await;
    pair_with(&service, &me, &beto).await;
    let id = send_to(&service, ana.node_id());

    let (status, body) = call(
        &service,
        &beto,
        "/peer/v1/receipts",
        &serde_json::json!({
            "read": [{ "id": id.to_string(), "read_at": "2026-09-20T10:14:00Z" }]
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "it is not an error, it is a no-op"
    );
    assert_eq!(body["recorded"], 0);
    let recipients = service.delivery_of(id).expect("ours").expect("we sent it");
    assert_eq!(recipients[0].node, ana.node_id());
    assert_eq!(
        recipients[0].state(),
        hivemind_core::store::Delivery::Queued
    );
}

#[tokio::test]
async fn an_empty_batch_and_an_unknown_message_are_both_accepted() {
    // A receipt for a message the sender has deleted is a fact with nowhere to
    // go, not a failure to report — and refusing it would make the courier
    // retry it forever.
    let me = identity(98);
    let (_dir, service) = service(&me);
    let ana = identity(99);
    pair_with(&service, &me, &ana).await;

    let (status, body) = call(&service, &ana, "/peer/v1/receipts", &serde_json::json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["recorded"], 0);

    let (status, body) = call(
        &service,
        &ana,
        "/peer/v1/receipts",
        &serde_json::json!({
            "read": [{ "id": ulid::Ulid::generate().to_string(), "read_at": "2026-09-20T10:14:00Z" }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["recorded"], 0);
}
