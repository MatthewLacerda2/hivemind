//! The handshake half of the peer router's tests: who is admitted, who is only
//! seen, and where they are recorded as being (SPEC §6.2, ADR 0013).
//!
//! Split from `peer.rs` when its tests passed the size gate, along the line
//! between "may this node talk to us" and "what may it do once it can".

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;

use super::tests::{call, caller_from, group_key, identity, join_group, offer, proof, service};
use super::{Handshake, router};

#[tokio::test]
async fn a_handshake_whose_body_disagrees_with_its_certificate_is_refused() {
    let host = identity(8);
    let caller_id = identity(9);
    let someone_else = identity(10);
    let (_dir, service) = service(&host);
    join_group(&service);

    // Claims to be someone else, with an otherwise perfect proof.
    let mut lie = offer(&caller_id, &host, "127.0.0.1", 8400, Some(&group_key()));
    lie["id"] = serde_json::Value::String(someone_else.node_id().to_string());
    let (status, problem) = call(&service, &caller_id, "/peer/v1/handshake", &lie).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], "/problems/identity-mismatch");
    assert!(!service.is_paired(caller_id.node_id()).expect("is_paired"));
    assert!(
        service.seen_nodes().expect("seen").is_empty(),
        "a node that misdescribes itself should not be recorded at all"
    );
}

#[tokio::test]
async fn a_handshake_proving_the_group_key_admits_the_caller_and_proves_back() {
    // ADR 0013: the key decides, and nobody is asked. The answer carries
    // our own proof, made for the caller, so it can admit us in turn.
    let host = identity(13);
    let friend = identity(14);
    let (_dir, service) = service(&host);
    join_group(&service);

    let (status, answer) = call(
        &service,
        &friend,
        "/peer/v1/handshake",
        &offer(&friend, &host, "127.0.0.1", 8400, Some(&group_key())),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(service.is_paired(friend.node_id()).expect("is_paired"));

    let answer: Handshake = serde_json::from_value(answer).expect("a handshake");
    let theirs = answer.proof.expect("the answer proves the key back");
    let mac = data_encoding::HEXLOWER
        .decode(theirs.mac.as_bytes())
        .expect("hex");
    assert_eq!(
        group_key().verify(
            host.certificate_der(),
            friend.certificate_der(),
            theirs.sent_at,
            &mac,
            chrono::Utc::now().timestamp_millis(),
        ),
        Ok(()),
        "the proof must be made from the host to this caller"
    );
}

#[tokio::test]
async fn a_handshake_without_the_group_key_is_refused_and_remembered_as_seen() {
    let host = identity(11);
    let stranger = identity(12);
    let (_dir, service) = service(&host);
    join_group(&service);

    let (status, problem) = call(
        &service,
        &stranger,
        "/peer/v1/handshake",
        &offer(&stranger, &host, "10.0.0.7", 9000, None),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(problem["type"], "/problems/not-paired");
    assert!(!service.is_paired(stranger.node_id()).expect("is_paired"));

    let seen = service.seen_nodes().expect("seen");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, stranger.node_id());
    assert_eq!(seen[0].name.as_deref(), Some("theirs"));
    assert_eq!(
        seen[0].addr.authority(),
        "10.0.0.2:9000",
        "the address the connection came from, with the port it claimed — \
         this used to record the claimed host, and every daemon claimed \
         its own loopback (#23)"
    );
}

#[tokio::test]
async fn a_handshake_proving_another_groups_key_is_refused() {
    let host = identity(15);
    let other = identity(16);
    let (_dir, service) = service(&host);
    join_group(&service);

    let theirs = hivemind_core::group::GroupKey::from_bytes([7u8; 16]);
    let (status, problem) = call(
        &service,
        &other,
        "/peer/v1/handshake",
        &offer(&other, &host, "127.0.0.1", 8400, Some(&theirs)),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("different groups")),
        "the refusal should say why: {problem}"
    );
    assert!(!service.is_paired(other.node_id()).expect("is_paired"));
}

#[tokio::test]
async fn a_proof_made_for_another_node_is_refused() {
    // A node that was greeted by a member holds a valid proof — made for
    // itself. Replaying it at somebody else must get nowhere.
    let host = identity(17);
    let replayer = identity(18);
    let original_receiver = identity(19);
    let (_dir, service) = service(&host);
    join_group(&service);

    let mut stolen = offer(&replayer, &host, "127.0.0.1", 8400, None);
    stolen["proof"] = serde_json::to_value(proof(&replayer, &original_receiver, &group_key()))
        .expect("serialise");
    let (status, _) = call(&service, &replayer, "/peer/v1/handshake", &stolen).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!service.is_paired(replayer.node_id()).expect("is_paired"));
}

#[tokio::test]
async fn a_node_in_no_group_admits_nobody() {
    let host = identity(20);
    let friend = identity(21);
    let (_dir, service) = service(&host);

    let (status, problem) = call(
        &service,
        &friend,
        "/peer/v1/handshake",
        &offer(&friend, &host, "127.0.0.1", 8400, Some(&group_key())),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("not in a group")),
        "the refusal should say this node has no group: {problem}"
    );
    assert!(!service.is_paired(friend.node_id()).expect("is_paired"));
}

#[tokio::test]
async fn a_peer_that_claims_loopback_from_elsewhere_is_not_believed() {
    // #23. Every daemon claimed `127.0.0.1`, so every peer learned its own
    // loopback as the way to reach the other one and then delivered to
    // itself. The socket knows where the connection came from; the claim
    // is only an opinion.
    let host = identity(40);
    let friend = identity(41);
    let (_dir, service) = service(&host);
    join_group(&service);

    let router =
        router(Arc::clone(&service)).layer(Extension(caller_from(&friend, "100.64.0.7:51234")));
    let request = Request::builder()
        .method("POST")
        .uri("/peer/v1/handshake")
        .header("content-type", "application/json")
        .body(Body::from(
            offer(&friend, &host, "127.0.0.1", 8400, Some(&group_key())).to_string(),
        ))
        .expect("request");
    let response = router.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        service.peer_addresses(friend.node_id()).expect("addresses")[0],
        "100.64.0.7:8400",
        "the address seen, with the port claimed — a source port is ephemeral"
    );
}

#[tokio::test]
async fn a_peer_genuinely_on_this_machine_keeps_its_loopback() {
    // Where `127.0.0.1` is true it stays true. This is the case every
    // integration test exercises, which is why #23 survived until two real
    // machines met.
    let host = identity(42);
    let neighbour = identity(43);
    let (_dir, service) = service(&host);
    join_group(&service);

    let router =
        router(Arc::clone(&service)).layer(Extension(caller_from(&neighbour, "127.0.0.1:51234")));
    let request = Request::builder()
        .method("POST")
        .uri("/peer/v1/handshake")
        .header("content-type", "application/json")
        .body(Body::from(
            offer(&neighbour, &host, "127.0.0.1", 9000, Some(&group_key())).to_string(),
        ))
        .expect("request");
    router.oneshot(request).await.expect("response");

    assert_eq!(
        service
            .peer_addresses(neighbour.node_id())
            .expect("addresses")[0],
        "127.0.0.1:9000"
    );
}

#[tokio::test]
async fn a_hostname_is_kept_because_it_may_resolve_where_we_cannot_see() {
    // A MagicDNS name is more useful than the IP behind it — it survives
    // the peer moving — so a non-IP claim is kept when the connection did
    // not come from loopback.
    let host = identity(44);
    let friend = identity(45);
    let (_dir, service) = service(&host);
    join_group(&service);

    let router =
        router(Arc::clone(&service)).layer(Extension(caller_from(&friend, "100.64.0.9:51234")));
    let request = Request::builder()
        .method("POST")
        .uri("/peer/v1/handshake")
        .header("content-type", "application/json")
        .body(Body::from(
            offer(
                &friend,
                &host,
                "laptop.tail1234.ts.net",
                8400,
                Some(&group_key()),
            )
            .to_string(),
        ))
        .expect("request");
    router.oneshot(request).await.expect("response");

    assert_eq!(
        service.peer_addresses(friend.node_id()).expect("addresses")[0],
        "laptop.tail1234.ts.net:8400"
    );
}
