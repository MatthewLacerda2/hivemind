//! The multipart body of a delivery (SPEC §7.2, §13.2).
//!
//! `POST /peer/v1/messages` is `multipart/form-data`: a `message` part and one
//! part per inline attachment. The parser sees these bytes before the
//! application has checked anything about them, so a malformed body must be a
//! `400`, never a panic.
//!
//! The boundary is fixed rather than fuzzed. A body whose boundary never
//! appears in it is rejected in one line and teaches nothing; holding the
//! boundary still is what makes the fuzzer spend its budget on framing —
//! truncated headers, missing terminators, parts with no disposition, a
//! declared length that does not match.

#![no_main]

use axum::body::Body;
use axum::extract::{FromRequest as _, Multipart};
use axum::http::{Request, header};
use libfuzzer_sys::fuzz_target;

const BOUNDARY: &str = "hivemind-fuzz";

fuzz_target!(|data: &[u8]| {
    // A current-thread runtime: this parses bytes, it does not do I/O, and a
    // thread pool per input would cost more than the parse.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime with no I/O driver always builds");

    runtime.block_on(async {
        let mut body = Vec::with_capacity(data.len() + 64);
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(data);

        let request = Request::builder()
            .method("POST")
            .uri("/peer/v1/messages")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .expect("the request is well-formed; only its body is fuzzed");

        let Ok(mut multipart) = Multipart::from_request(request, &()).await else {
            return;
        };

        // Walk it the way the delivery handler does, including reading each
        // part's bytes -- a header that parses and a body that does not is
        // exactly the shape worth finding.
        while let Ok(Some(field)) = multipart.next_field().await {
            let _ = field.name().map(ToOwned::to_owned);
            let _ = field.file_name().map(ToOwned::to_owned);
            let Ok(bytes) = field.bytes().await else {
                return;
            };
            // The handler parses the `message` part as JSON. Anything else is
            // matched against a digest, which cannot panic.
            let _ = serde_json::from_slice::<hivemind_core::message::Message>(&bytes);
        }
    });
});
