//! The blob half of the peer API's tests (SPEC §7.2).
//!
//! Its own file because `peer.rs` grew past the test-size gate, and because
//! serving bytes with resume is a different concern from introducing two nodes
//! to each other. Included from `peer.rs` with `#[path]` so the tests still
//! reach the private handlers they are about.

use hivemind_core::identity::Identity;

use super::tests::{caller, identity, pair_with, service};
use super::*;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

/// GET or HEAD the peer blob route as `who`, with optional headers.
async fn fetch(
    service: &Arc<MailService>,
    who: &Identity,
    method: &str,
    sha: &str,
    range: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let router = router(Arc::clone(service)).layer(Extension(caller(who)));
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/peer/v1/blobs/{sha}"));
    if let Some(range) = range {
        request = request.header("range", range);
    }

    let response = router
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

fn header(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
}

#[tokio::test]
async fn a_paired_peer_can_fetch_a_whole_blob() {
    let host = identity(15);
    let friend = identity(16);
    let (_dir, service) = service(&host);
    pair_with(&service, &friend).await;

    let digest = service.blobs().put_bytes(b"attachment bytes").expect("put");

    let (status, headers, body) = fetch(&service, &friend, "GET", &digest.to_hex(), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"attachment bytes");
    assert_eq!(header(&headers, "accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(header(&headers, "content-length").as_deref(), Some("16"));
}

#[tokio::test]
async fn an_interrupted_transfer_resumes_from_where_it_stopped() {
    // SPEC §7.2: range requests supported (resume). This is the wire half
    // of what the blob store does on disk.
    let host = identity(17);
    let friend = identity(18);
    let (_dir, service) = service(&host);
    pair_with(&service, &friend).await;

    let content = b"0123456789abcdef";
    let digest = service.blobs().put_bytes(content).expect("put");

    let (status, headers, body) = fetch(
        &service,
        &friend,
        "GET",
        &digest.to_hex(),
        Some("bytes=10-"),
    )
    .await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"abcdef", "only the part that was missing");
    assert_eq!(
        header(&headers, "content-range").as_deref(),
        Some("bytes 10-15/16")
    );
    assert_eq!(header(&headers, "content-length").as_deref(), Some("6"));
}

#[tokio::test]
async fn resuming_past_the_end_says_so_rather_than_hanging() {
    // The two sides disagree about the file. 416 tells the caller to start
    // over; an empty 206 would leave it waiting for bytes never coming.
    let host = identity(19);
    let friend = identity(20);
    let (_dir, service) = service(&host);
    pair_with(&service, &friend).await;

    let digest = service.blobs().put_bytes(b"short").expect("put");

    let (status, headers, _) = fetch(
        &service,
        &friend,
        "GET",
        &digest.to_hex(),
        Some("bytes=99-"),
    )
    .await;

    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        header(&headers, "content-range").as_deref(),
        Some("bytes */5")
    );
}

#[tokio::test]
async fn asking_whether_a_blob_is_still_there_does_not_send_it() {
    let host = identity(21);
    let friend = identity(22);
    let (_dir, service) = service(&host);
    pair_with(&service, &friend).await;

    let digest = service.blobs().put_bytes(b"still here").expect("put");

    let (status, headers, body) = fetch(&service, &friend, "HEAD", &digest.to_hex(), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "content-length").as_deref(), Some("10"));
    assert!(body.is_empty(), "HEAD has no body");

    let gone = hivemind_core::crypto::Sha256Digest::of(b"deleted since");
    let (status, _, _) = fetch(&service, &friend, "HEAD", &gone.to_hex(), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a sender that deleted the file should say so, not stall the download"
    );
}

#[tokio::test]
async fn an_unpaired_peer_cannot_fetch_anything() {
    // Blobs are only fetched for mail the peer already received, so this
    // takes the same check as delivery.
    let host = identity(23);
    let stranger = identity(24);
    let (_dir, service) = service(&host);

    let digest = service.blobs().put_bytes(b"private").expect("put");

    let (status, _, _) = fetch(&service, &stranger, "GET", &digest.to_hex(), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, _) = fetch(&service, &stranger, "HEAD", &digest.to_hex(), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_digest_that_is_not_a_digest_cannot_name_a_file() {
    // It arrives in a URL path from another machine.
    let host = identity(25);
    let friend = identity(26);
    let (_dir, service) = service(&host);
    pair_with(&service, &friend).await;

    for attempt in ["..", "not-hex", "%2e%2e%2fpeers.toml"] {
        let (status, _, _) = fetch(&service, &friend, "GET", attempt, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{attempt:?} should not name anything"
        );
    }
}

#[test]
fn only_a_resume_range_is_honoured() {
    let range = |value: &str| {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("range", value.parse().expect("header"));
        resume_offset(&headers)
    };

    assert_eq!(range("bytes=1024-"), Some(1024));
    assert_eq!(range("bytes=0-"), Some(0));
    // Everything else gets the whole blob, which is correct if unhelpful.
    assert_eq!(range("bytes=0-99"), None);
    assert_eq!(range("bytes=-500"), None);
    assert_eq!(range("bytes=0-10,20-30"), None);
    assert_eq!(range("items=1-"), None);
    assert_eq!(resume_offset(&axum::http::HeaderMap::new()), None);
}
