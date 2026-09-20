//! What the pages say (SPEC §11).
//!
//! Its own file for two reasons. `web.rs` was two lines under the test-size
//! gate once these were written, and the split is along a boundary that was
//! already there: everything here goes through the router and asserts on the
//! markup that came back, while what stays in `web.rs` is three functions that
//! take a value and return a string. A child of `web`, so the tests still reach
//! the private handlers they are about.

use super::*;
use axum::body::Body;
use axum::http::Request;
use hivemind_core::peer::NodeId;
use hivemind_core::peerbook::PeerAddr;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use crate::service::NodeDescription;

pub(super) fn app() -> (tempfile::TempDir, Router, Arc<MailService>) {
    let dir = tempfile::tempdir().expect("temp dir");
    // A real certificate and key. With the placeholder that stood here,
    // anything reaching the network failed while building the TLS
    // configuration, so a test about an address that does not answer never
    // contacted an address at all (#57).
    let identity = hivemind_core::identity::Identity::from_seed([3u8; 32]).expect("identity");
    let node = NodeDescription {
        id: identity.node_id(),
        certificate: identity.certificate_der().to_vec(),
        private_key: identity.private_key_pkcs8().expect("key"),
        name: "test".to_owned(),
        owner: Some("tester".to_owned()),
        callback_host: "127.0.0.1".to_owned(),
        peer_port: 8400,
        max_attachment_bytes: hivemind_core::config::DEFAULT_MAX_ATTACHMENT_BYTES,
        inline_max_bytes: hivemind_core::config::DEFAULT_INLINE_MAX_BYTES,
        prefetch: false,
        presence_interval: hivemind_core::config::DEFAULT_PRESENCE_INTERVAL,
        // Off, not `Auto`: "look for peers now" runs the real `tailscale`
        // binary under `Auto`, so what the page does next would depend on
        // whether the machine running the tests has a tailnet. Discovery
        // has its own tests against a named backend; the UI's are about
        // the page.
        tailscale: hivemind_core::config::Tailscale::Off,
    };
    let service = Arc::new(
        MailService::open(dir.path(), node, identity.signing_key().clone()).expect("service"),
    );
    (dir, router(Arc::clone(&service)), service)
}

/// An answer from the UI, as a reader sees it.
///
/// Every request in this module comes back as one of these, and the
/// assertions look at [`Page::content`] rather than at the whole document.
/// That is the point of it: `document.contains(…)` is answered by the
/// layout — the navigation names all four pages and the node id is on every
/// one of them — so a page whose own content went missing still passes such
/// a check. A sweep of this module found thirteen survivors in sixty-two
/// mutants living in that gap (#101), because the only thing asserted about
/// most pages was that they had a `<main>`, and an empty peers table has one.
pub(super) struct Page {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    html: String,
}

impl Page {
    /// What is inside `<main>`: this page's own content, without the layout
    /// every page shares. Empty when the page rendered no `<main>` at all,
    /// so an assertion on a page that failed to render fails rather than
    /// matching some of the chrome.
    fn content(&self) -> &str {
        self.html
            .split_once("<main id=\"main\">")
            .and_then(|(_, rest)| rest.split_once("</main>"))
            .map_or("", |(inside, _)| inside)
    }

    /// The whole answer. For the error pages, which are built by hand
    /// rather than by askama and have no layout to look inside, and for the
    /// assets, which are not pages.
    fn document(&self) -> &str {
        &self.html
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Where this answer sends the browser next. Reading works with
    /// JavaScript switched off, so every action is a post that redirects.
    fn redirect(&self) -> Option<&str> {
        self.header("location")
    }

    #[track_caller]
    pub(super) fn says(&self, text: &str) -> &Self {
        assert!(
            self.content().contains(text),
            "the page should say {text:?} and says: {}",
            self.content()
        );
        self
    }

    #[track_caller]
    pub(super) fn does_not_say(&self, text: &str) -> &Self {
        assert!(
            !self.content().contains(text),
            "the page should not say {text:?} and says: {}",
            self.content()
        );
        self
    }

    /// The content below a heading, for a page carrying more than one list.
    #[track_caller]
    fn below(&self, heading: &str) -> &str {
        let content = self.content();
        content.split_once(heading).map_or_else(
            || panic!("the page has no {heading:?} on it: {content}"),
            |(_, rest)| rest,
        )
    }
}

async fn fetch(router: &Router, request: Request<Body>) -> Page {
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    Page {
        status,
        headers,
        html: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

pub(super) async fn get(router: &Router, path: &str) -> Page {
    let request = Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("request");
    fetch(router, request).await
}

/// A POST with no body, for the routes whose whole input is the path.
async fn post(router: &Router, path: &str) -> Page {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .body(Body::empty())
        .expect("request");
    fetch(router, request).await
}

async fn post_form(router: &Router, path: &str, form: &str) -> Page {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .expect("request");
    fetch(router, request).await
}

const BOUNDARY: &str = "----------hivemind";

/// The compose form as a browser with no JavaScript posts it: one part per
/// field, in order, with a `filename` on the parts that carry files.
async fn post_parts(router: &Router, path: &str, parts: &[(&str, Option<&str>, &str)]) -> Page {
    use std::fmt::Write as _;

    let mut body = String::new();
    for (name, filename, value) in parts {
        let disposition = filename.map_or_else(
            || format!("name=\"{name}\""),
            |filename| format!("name=\"{name}\"; filename=\"{filename}\""),
        );
        write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; {disposition}\r\n\r\n{value}\r\n"
        )
        .expect("a string never fails to grow");
    }
    body.push_str("--");
    body.push_str(BOUNDARY);
    body.push_str("--\r\n");

    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("request");
    fetch(router, request).await
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
        .message
        .id
}

/// A member of this node's group, as a handshake would have left it.
fn admitted(service: &Arc<MailService>) -> NodeId {
    let friend = hivemind_core::identity::Identity::from_seed([29u8; 32]).expect("identity");
    let id = friend.node_id();
    service
        .admit(
            id,
            "their-laptop",
            Some("leonardo"),
            friend.certificate_der().to_vec(),
            PeerAddr::manual("10.0.0.9", 8400),
        )
        .expect("admit");
    id
}

#[tokio::test]
async fn the_inbox_reads_without_any_javascript() {
    // SPEC §11. Not a box-tick: this is what makes the UI usable when the
    // daemon is up and something in the page is broken.
    let (_dir, router, service) = app();
    send_to_self(&service, "a subject", "a body");

    let page = get(&router, "/").await;

    assert_eq!(page.status, StatusCode::OK);
    page.says("a subject");
    // The one <script> is deferred and optional; nothing above depends on
    // it. If the list ever moves into JS, this fails.
    let without_scripts = page
        .document()
        .split("<script")
        .next()
        .expect("there is always a first part");
    assert!(
        without_scripts.contains("a subject"),
        "the list must be rendered before any script tag"
    );
}

#[tokio::test]
async fn every_page_says_what_it_is_for() {
    // A marker per page, taken from inside `<main>`. This was a check for
    // a `<main>` and a `lang`, which every page has whether or not it
    // rendered anything of its own — the peers table could come back empty
    // and it still passed (#101). A page added to the UI adds its marker
    // here.
    let (_dir, router, service) = app();
    let id = send_to_self(&service, "rendered", "what it said");
    let (_, message) = service.get(id).expect("get");

    for (path, marker) in [
        ("/", "rendered"),
        ("/sent", "rendered"),
        ("/compose", "name=\"subject\""),
        ("/peers", "action=\"/peers/join\""),
        (&format!("/thread/{}", message.thread_id), "what it said"),
    ] {
        let page = get(&router, path).await;
        assert_eq!(page.status, StatusCode::OK, "{path} should render");
        assert!(
            page.document().contains("lang=\"en\""),
            "{path} needs a language"
        );
        page.says(marker);
    }
}

#[tokio::test]
async fn the_compose_form_sends_what_was_typed_into_it() {
    // The only way to send mail from the UI, and it had no test at all: the
    // whole handler was replaceable by an empty 200, and each of its four
    // multipart fields could be dropped, with the suite still green (#101).
    let (_dir, router, service) = app();

    let page = post_parts(
        &router,
        "/compose",
        &[
            ("to", None, &service.identity().to_string()),
            ("subject", None, "a posted subject"),
            ("body", None, "a posted body"),
            ("files", Some("notes.txt"), "hello"),
        ],
    )
    .await;

    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.document());
    let thread = page
        .redirect()
        .expect("a redirect to the thread")
        .to_owned();

    // Post-and-redirect, so the thread it names is where the message is.
    let thread = get(&router, &thread).await;
    thread
        .says("a posted subject")
        .says("a posted body")
        .says("notes.txt")
        .says("5 B");
    get(&router, "/sent").await.says("a posted subject");
}

#[tokio::test]
async fn a_recipient_the_node_does_not_know_keeps_the_half_written_message() {
    // Losing a body to a typo in the recipient would be its own small
    // tragedy, and the re-render is also what tells an empty 200 from a
    // page: the handler could answer `Default::default()` unnoticed.
    let (_dir, router, _service) = app();

    let page = post_parts(
        &router,
        "/compose",
        &[
            ("to", None, "nobody-here"),
            ("subject", None, "kept"),
            ("body", None, "worth keeping"),
        ],
    )
    .await;

    assert_eq!(page.redirect(), None, "nothing was sent");
    page.says("nobody-here").says("worth keeping");
}

#[tokio::test]
async fn a_form_that_cannot_be_read_says_so_rather_than_failing_quietly() {
    let (_dir, router, _service) = app();
    let request = Request::builder()
        .method("POST")
        .uri("/compose")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from("not a multipart body at all"))
        .expect("request");

    let page = fetch(&router, request).await;

    assert_eq!(page.status, StatusCode::BAD_REQUEST);
    assert!(
        page.document().contains("That form could not be read"),
        "{}",
        page.document()
    );
}

#[tokio::test]
async fn a_search_lists_what_matches_and_nothing_else() {
    // The filter that treats a blank search as no search at all: deleting
    // its `!` inverted it, and no test searched for anything (#101).
    let (_dir, router, service) = app();
    send_to_self(&service, "the standup", "one");
    send_to_self(&service, "the release", "two");

    get(&router, "/?q=standup")
        .await
        .says("the standup")
        .does_not_say("the release");
    // A search box somebody pressed return in without typing is not a
    // search for whitespace.
    get(&router, "/?q=%20%20")
        .await
        .says("the standup")
        .says("the release");
}

#[tokio::test]
async fn mail_not_yet_read_is_marked_in_the_listing_and_stops_being() {
    // The one thing the listing says about a message that the API does not:
    // the marker came from a comparison against `New` that survived being
    // inverted, because nothing looked at the class (#101).
    let (_dir, router, service) = app();
    let id = send_to_self(&service, "unread", "body");
    let (_, message) = service.get(id).expect("get");

    get(&router, "/").await.says("class=\"message unread\"");

    get(&router, &format!("/thread/{}", message.thread_id)).await;

    get(&router, "/")
        .await
        .does_not_say("class=\"message unread\"");
}

#[tokio::test]
async fn the_peers_page_says_what_to_do_when_this_node_is_in_no_group() {
    // Without a group this machine can reach nobody, which is the first
    // thing somebody opening the page needs to know (ADR 0013).
    let (_dir, router, service) = app();
    get(&router, "/peers")
        .await
        .says("Not in a group yet")
        .says("hivemind group create");

    service.create_group(false).expect("create");
    get(&router, "/peers")
        .await
        .does_not_say("Not in a group yet");
}

#[tokio::test]
async fn the_peers_page_lists_a_member_with_a_way_to_reach_it() {
    // The table could come back empty — `peer_rows` replaced by `vec![]`
    // survived — and the page has no error to show for it: a group with
    // nobody in it looks exactly like a group whose members went missing
    // (#101).
    let (_dir, router, service) = app();
    let id = admitted(&service);

    let page = get(&router, "/peers").await;
    let group = page.below("In the group");
    for expected in [
        "their-laptop",
        "leonardo",
        "10.0.0.9:8400",
        &id.to_string(),
        &format!("Forget {}", id.short()),
    ] {
        assert!(group.contains(expected), "{expected} is missing: {group}");
    }
    page.does_not_say("Nobody yet");
}

#[tokio::test]
async fn a_node_seen_outside_the_group_is_listed_apart_from_members() {
    let (_dir, router, service) = app();
    service.record_seen(
        NodeId::from_certificate_der(b"somebody else"),
        Some("a-stranger".to_owned()),
        None,
        PeerAddr::manual("10.0.0.10", 8400),
    );

    let page = get(&router, "/peers").await;
    assert!(
        page.below("Seen, not in the group").contains("a-stranger"),
        "{}",
        page.content()
    );
    let members = page
        .below("In the group")
        .split("Seen, not in the group")
        .next()
        .expect("there is always a first part")
        .to_owned();
    assert!(
        !members.contains("a-stranger"),
        "a node outside the group does not belong among the members: {members}"
    );
}

#[tokio::test]
async fn forgetting_a_peer_from_the_page_returns_to_a_list_without_it() {
    // `forget` answering an empty 200 survived: the redirect is the only
    // thing that tells "gone" from "the button did nothing".
    let (_dir, router, service) = app();
    let id = admitted(&service);

    let page = post(&router, &format!("/peers/{id}/remove")).await;

    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.redirect(), Some("/peers"));
    get(&router, "/peers").await.does_not_say("their-laptop");
}

#[tokio::test]
async fn looking_for_peers_now_goes_back_to_the_list() {
    // With Tailscale off this finds nobody, which is the case worth
    // asserting: nothing found is still a refresh that worked, and the page
    // must not complain about it. The handler was replaceable by an empty
    // 200 (#101).
    let (_dir, router, _service) = app();

    let page = post(&router, "/peers/refresh").await;

    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.redirect(), Some("/peers"));
}

#[tokio::test]
async fn contacting_an_address_that_does_not_answer_stays_on_the_page_and_says_so() {
    // A redirect to the list would look as though it had worked.
    let (_dir, router, _service) = app();
    let page = post_form(&router, "/peers/join", "host=127.0.0.1%3A1").await;

    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.redirect(),
        None,
        "no redirect when nothing was reached"
    );
    // The status and the missing redirect are also what an empty answer
    // looks like, so the page has to be there and has to carry the
    // complaint.
    page.says("127.0.0.1:1");
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

    get(&router, "/")
        .await
        .says("<span class=\"badge agent\">agent</span>");
}

#[tokio::test]
async fn a_reply_posted_from_a_form_redirects_back_to_the_thread() {
    // No JavaScript means post-and-redirect, so reloading the page does
    // not send the reply twice.
    let (_dir, router, service) = app();
    let id = send_to_self(&service, "question", "what time?");
    let (_, message) = service.get(id).expect("get");

    let page = post_form(
        &router,
        &format!("/thread/{}/reply", message.thread_id),
        "body=one+o%27clock",
    )
    .await;

    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(
        page.redirect(),
        Some(format!("/thread/{}", message.thread_id).as_str())
    );

    let thread = get(&router, &format!("/thread/{}", message.thread_id)).await;
    thread.says("one o").says("clock");
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

    get(&router, "/")
        .await
        .does_not_say("<script>alert(1)</script>")
        // And it should still be readable.
        .says("&#60;script&#62;");
}

#[tokio::test]
async fn a_body_cannot_smuggle_markup_into_a_thread() {
    let (_dir, router, service) = app();
    let id = send_to_self(&service, "innocent", "<img src=x onerror=alert(1)>");
    let (_, message) = service.get(id).expect("get");

    get(&router, &format!("/thread/{}", message.thread_id))
        .await
        .does_not_say("<img src=x")
        .says("&#60;img src=x");
}

#[tokio::test]
async fn the_stylesheet_is_served_from_the_binary() {
    let (_dir, router, _) = app();
    let page = get(&router, "/assets/hivemind.css").await;

    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.document().contains("prefers-color-scheme"),
        "SPEC §11 asks for it"
    );
    assert!(
        page.document().contains(":focus-visible"),
        "and keyboard navigation"
    );
    assert_eq!(page.header("content-type"), Some("text/css; charset=utf-8"));
    assert_eq!(page.header("cache-control"), Some("no-cache"));
}

#[tokio::test]
async fn an_asset_path_cannot_escape_the_bundle() {
    let (_dir, router, _) = app();
    for attempt in [
        "/assets/..%2f..%2fpeers.toml",
        "/assets/../../peers.toml",
        "/assets/nothing.css",
    ] {
        let page = get(&router, attempt).await;
        assert_eq!(
            page.status,
            StatusCode::NOT_FOUND,
            "{attempt} should be refused"
        );
    }
}

#[tokio::test]
async fn an_error_page_escapes_what_it_quotes() {
    // The error pages are built by hand rather than by askama, so the
    // escaping is this module's own and nothing was asserting it: a sweep
    // replaced `escape` with the empty string and with "xyzzy", and both
    // survived (#57). The id in the path reaches the page through the
    // error's detail, so it is attacker-controlled text.
    let (_dir, router, _service) = app();

    let page = post(&router, "/peers/%3Cscript%3Ealert(1)%3C%2Fscript%3E/remove").await;

    // 403, because an id this node does not know is an id it is not
    // paired with (SPEC §7.3).
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    assert!(
        !page.document().contains("<script>"),
        "the id must not reach the page as markup: {}",
        page.document()
    );
    assert!(
        page.document()
            .contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "and it should still say which id was asked for: {}",
        page.document()
    );
}

#[tokio::test]
async fn a_thread_that_does_not_exist_is_a_404_not_a_500() {
    let (_dir, router, _) = app();
    for path in ["/thread/not-a-ulid", "/thread/01JXT2ZZZZZZZZZZZZZZZZZZZZ"] {
        let page = get(&router, path).await;
        assert_eq!(page.status, StatusCode::NOT_FOUND, "{path}");
        assert!(
            page.document().contains("no such thread"),
            "{path}: {}",
            page.document()
        );
    }
}
