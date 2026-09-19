//! The presence half of the peer router's tests (SPEC §5.5).
//!
//! What a hello has to do that a handshake does not: check the proof *again*
//! so a rotated key takes effect, carry the peer list, wake delivery, and
//! refuse — and retire — a node that can no longer prove membership.

use std::sync::Arc;

use axum::http::StatusCode;
use hivemind_core::identity::Identity;
use hivemind_core::peer::NodeId;
use hivemind_core::peerbook::{AddrSource, PeerAddr};

use super::tests::{call, group_key, identity, join_group, offer, pair_with, proof, service};
use super::{Hello, PeerNote};
use crate::service::MailService;

/// A hello from `from` to `to`, claiming `host:port`, proving `key`.
fn greeting(
    from: &Identity,
    to: &Identity,
    key: Option<&hivemind_core::group::GroupKey>,
) -> serde_json::Value {
    serde_json::to_value(Hello {
        id: from.node_id().to_string(),
        name: "theirs".to_owned(),
        owner: Some("someone".to_owned()),
        version: "0.1.0".to_owned(),
        callback_host: "10.0.0.2".to_owned(),
        callback_port: 8400,
        proof: key.map(|key| proof(from, to, key)),
        peers: Vec::new(),
        sessions: Vec::new(),
        up: Vec::new(),
    })
    .expect("serialise")
}

/// The same, carrying a peer list.
fn greeting_about(from: &Identity, to: &Identity, peers: Vec<PeerNote>) -> serde_json::Value {
    let mut hello = greeting(from, to, Some(&group_key()));
    hello["peers"] = serde_json::to_value(peers).expect("serialise");
    hello
}

#[tokio::test]
async fn a_hello_proving_the_key_marks_the_sender_online_and_answers_in_kind() {
    let host = identity(60);
    let friend = identity(61);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    assert!(
        !service.is_online(friend.node_id()),
        "a peer is not online merely for being in peers.toml"
    );

    let (status, answer) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting(&friend, &host, Some(&group_key())),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(service.is_online(friend.node_id()));

    // The answer is a hello, so one request tells both sides about each other.
    let answer: Hello = serde_json::from_value(answer).expect("a hello back");
    assert_eq!(answer.id, host.node_id().to_string());
    assert!(
        answer.proof.is_some(),
        "the answer proves the key back, or the other side cannot keep us"
    );
}

#[tokio::test]
async fn a_member_that_can_no_longer_prove_the_key_is_refused_and_dropped() {
    // SPEC §6.2.4 and the whole reason presence re-checks: the proof at
    // pairing time can never be withdrawn, so without this a node rotated
    // out of the group would go on delivering forever.
    let host = identity(62);
    let friend = identity(63);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;
    assert!(service.is_paired(friend.node_id()).expect("is_paired"));

    // The host rotates the key; the friend still holds the old one.
    let stale = group_key();
    service
        .create_group(true)
        .expect("a new key replaces the old");

    let (status, problem) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting(&friend, &host, Some(&stale)),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(problem["type"], "/problems/not-paired");
    assert!(
        !service.is_paired(friend.node_id()).expect("is_paired"),
        "it stops receiving mail, which means leaving peers.toml"
    );
    assert!(!service.is_online(friend.node_id()));
    assert!(
        service
            .seen_nodes()
            .expect("seen")
            .iter()
            .any(|node| node.id == friend.node_id()),
        "and it is still a node we have met, listed as seen"
    );
}

#[tokio::test]
async fn a_dropped_member_is_remembered_as_one_so_later_rounds_retry_it() {
    // A rotation drops every member that has not pasted the new code yet,
    // including the ones about to. Two members that drop *each other* both
    // stop being peers, and a presence round greets peers — so without this
    // nothing would ever greet either of them again. On a tailnet, with no
    // mDNS to find anybody a second time, that partition is permanent.
    let host = identity(92);
    let friend = identity(93);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    let stale = group_key();
    service.create_group(true).expect("a new key");

    let (status, _) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting(&friend, &host, Some(&stale)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let seen = service.seen_nodes().expect("seen");
    let node = seen
        .iter()
        .find(|node| node.id == friend.node_id())
        .expect("it is on the seen list");
    assert!(
        node.was_a_member,
        "it was in the group a moment ago, and is one pasted code from being in it again"
    );
}

#[tokio::test]
async fn a_stranger_that_was_never_a_member_is_not_marked_as_one() {
    // The other half: presence retries ex-members and leaves strangers
    // alone, so the two have to be distinguishable. A node from another
    // group would otherwise be greeted once a minute forever.
    let host = identity(94);
    let stranger = identity(95);
    let (_dir, service) = service(&host);
    join_group(&service);

    let other_group = hivemind_core::group::GroupKey::from_bytes([7u8; 16]);
    let (status, _) = call(
        &service,
        &stranger,
        "/peer/v1/handshake",
        &offer(&stranger, &host, "127.0.0.1", 8400, Some(&other_group)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let seen = service.seen_nodes().expect("seen");
    let node = seen
        .iter()
        .find(|node| node.id == stranger.node_id())
        .expect("it is on the seen list");
    assert!(!node.was_a_member);
}

#[tokio::test]
async fn a_hello_with_no_proof_at_all_is_refused() {
    let host = identity(64);
    let friend = identity(65);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    let (status, _) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting(&friend, &host, None),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!service.is_paired(friend.node_id()).expect("is_paired"));
}

#[tokio::test]
async fn a_hello_whose_body_disagrees_with_its_certificate_is_refused() {
    let host = identity(66);
    let friend = identity(67);
    let someone_else = identity(68);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    let mut lie = greeting(&friend, &host, Some(&group_key()));
    lie["id"] = serde_json::Value::String(someone_else.node_id().to_string());
    let (status, problem) = call(&service, &friend, "/peer/v1/hello", &lie).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], "/problems/identity-mismatch");
    assert!(
        service.is_paired(friend.node_id()).expect("is_paired"),
        "a body that disagrees is not evidence about the group key"
    );
}

#[tokio::test]
async fn a_hello_wakes_delivery_for_the_peer_that_sent_it() {
    // SPEC §8's Monday morning. Without this the queued mail waits out a
    // backoff of up to five minutes for a laptop that is demonstrably up.
    let host = identity(69);
    let friend = identity(70);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    assert!(
        service.take_woken().is_empty(),
        "nothing is woken before anybody says hello"
    );

    let (status, _) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting(&friend, &host, Some(&group_key())),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert!(
        service.take_woken().contains(&friend.node_id()),
        "the sender's queue should go now, not at the end of its backoff"
    );
    assert!(
        service.take_woken().is_empty(),
        "and a wake is one instruction, not a standing exemption from backoff"
    );
}

#[tokio::test]
async fn a_peer_list_tops_up_the_addresses_of_a_peer_we_already_know() {
    let host = identity(71);
    let friend = identity(72);
    let third = identity(73);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;
    pair_with(&service, &host, &third).await;

    let (status, _) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting_about(
            &friend,
            &host,
            vec![PeerNote {
                id: third.node_id().to_string(),
                name: "third".to_owned(),
                owner: None,
                addrs: vec!["10.9.9.9:8400".to_owned()],
            }],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let learned = addresses_of(&service, third.node_id());
    assert!(
        learned
            .iter()
            .any(|addr| addr.host == "10.9.9.9" && addr.source == AddrSource::Gossip),
        "the address should be in the book, marked as gossip: {learned:?}"
    );
}

#[tokio::test]
async fn a_peer_list_about_a_stranger_buys_an_attempt_and_nothing_else() {
    // SPEC §5.4: nothing learned by gossip is trusted beyond "try this
    // address". A member that lied here could otherwise add peers to
    // somebody else's address book by saying they exist.
    let host = identity(74);
    let friend = identity(75);
    let stranger = identity(76);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    let (status, _) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting_about(
            &friend,
            &host,
            vec![PeerNote {
                id: stranger.node_id().to_string(),
                name: "stranger".to_owned(),
                owner: None,
                addrs: vec!["10.8.8.8:8400".to_owned()],
            }],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert!(
        !service.is_paired(stranger.node_id()).expect("is_paired"),
        "being named by a member is not membership"
    );
    assert_eq!(
        service.take_candidates(),
        vec!["10.8.8.8:8400".to_owned()],
        "it is an address worth greeting, which is all"
    );
}

#[tokio::test]
async fn our_own_node_is_never_taken_from_somebody_elses_peer_list() {
    // Every other member has us in its list, so this arrives on every hello.
    // Greeting ourselves is caught further down, but paying for it once a
    // minute per peer is not a thing to leave to the next check.
    let host = identity(77);
    let friend = identity(78);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    let (status, _) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting_about(
            &friend,
            &host,
            vec![PeerNote {
                id: host.node_id().to_string(),
                name: "host".to_owned(),
                owner: None,
                addrs: vec!["10.7.7.7:8400".to_owned()],
            }],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(service.take_candidates().is_empty());
}

#[tokio::test]
async fn a_hint_about_a_peer_we_think_is_down_queues_our_own_hello() {
    // SPEC §5.5: a hint is never believed. It buys one request, and it is
    // the answer to *that* which marks the node online.
    let host = identity(79);
    let friend = identity(80);
    let third = identity(81);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;
    pair_with(&service, &host, &third).await;

    let mut hint = greeting(&friend, &host, Some(&group_key()));
    hint["up"] = serde_json::json!([third.node_id().to_string()]);

    let (status, _) = call(&service, &friend, "/peer/v1/hello", &hint).await;
    assert_eq!(status, StatusCode::OK);

    assert!(
        !service.is_online(third.node_id()),
        "the hint alone says nothing"
    );
    assert!(
        !service.take_candidates().is_empty(),
        "but it is worth one hello of our own"
    );
}

#[tokio::test]
async fn a_hint_about_a_peer_already_online_costs_nothing() {
    let host = identity(82);
    let friend = identity(83);
    let third = identity(84);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;
    pair_with(&service, &host, &third).await;
    service.mark_online(third.node_id(), Vec::new());

    let mut hint = greeting(&friend, &host, Some(&group_key()));
    hint["up"] = serde_json::json!([third.node_id().to_string()]);

    let (status, _) = call(&service, &friend, "/peer/v1/hello", &hint).await;
    assert_eq!(status, StatusCode::OK);
    assert!(service.take_candidates().is_empty());
}

#[tokio::test]
async fn who_we_heard_from_is_passed_on_in_the_next_hello_we_send() {
    let host = identity(85);
    let friend = identity(86);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    let (status, _) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting(&friend, &host, Some(&group_key())),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // That same answer carried the hint back, which is a round trip nobody
    // needs — so what matters is that the *next* one we compose still has it
    // for everybody else.
    let onward = service
        .own_hello(identity(87).certificate_der())
        .expect("a hello");
    assert!(
        onward.up.contains(&friend.node_id().to_string()),
        "the rest of the group should hear that it is up: {:?}",
        onward.up
    );
}

#[tokio::test]
async fn nobody_is_told_that_they_themselves_are_up() {
    // The first hello composed after hearing from a peer is the answer to
    // that peer. Telling it what it just told us is a round trip that teaches
    // nothing, and it is how the hint used to be spent before anybody else
    // could see it.
    let host = identity(90);
    let friend = identity(91);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    let (status, answer) = call(
        &service,
        &friend,
        "/peer/v1/hello",
        &greeting(&friend, &host, Some(&group_key())),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let answer: Hello = serde_json::from_value(answer).expect("a hello back");
    assert!(
        !answer.up.contains(&friend.node_id().to_string()),
        "it knows it is up; it just said so: {:?}",
        answer.up
    );
}

#[tokio::test]
async fn our_hello_carries_the_group_as_we_know_it() {
    let host = identity(88);
    let friend = identity(89);
    let (_dir, service) = service(&host);
    pair_with(&service, &host, &friend).await;

    let hello = service
        .own_hello(friend.certificate_der())
        .expect("a hello");

    let note = hello
        .peers
        .iter()
        .find(|note| note.id == friend.node_id().to_string())
        .expect("the peer list names the peer we have");
    assert!(
        !note.addrs.is_empty(),
        "and says where it is, or it teaches nobody anything"
    );
}

/// The address book's entry for `id`.
fn addresses_of(service: &Arc<MailService>, id: NodeId) -> Vec<PeerAddr> {
    service
        .paired_peers()
        .expect("peers")
        .into_iter()
        .find(|peer| peer.id == id)
        .expect("a peer")
        .addrs
}
