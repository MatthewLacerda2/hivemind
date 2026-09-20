//! The delivery log, asserted on rather than hoped for (#32).
//!
//! The worker used to log one `debug` line per failed address, which at the
//! level the daemon runs at meant it logged nothing: two hours of real use over
//! three machines left seven lines in `daemon.log`, all of them from startup,
//! and a message that was quietly backing off was written up as stuck. The lines
//! these tests pin down are behaviour, so they are tested like behaviour — a log
//! with no test is a log that vanishes in the next refactor.
//!
//! It lives here rather than beside the code because the capture harness and its
//! cases are a concern of their own, and `delivery.rs` is one of the larger test
//! files in the repository already.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use hivemind_core::crypto::{Signature, SigningKey};
use hivemind_core::message::{Kind, Message, Recipient, SenderKind};
use hivemind_core::peer::NodeId;
use hivemind_core::store::{Outbound, RecipientState};
use hivemind_net::client::ClientError;
use hivemind_net::delivery::{Transport, attempt_all};

fn node(seed: u8) -> NodeId {
    NodeId::from_certificate_der(&[seed; 16])
}

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).expect("in range")
}

/// One message owed to one peer.
fn outbound_to(peer: NodeId) -> Outbound {
    let id = ulid::Ulid::generate();
    let mut message = Message {
        id,
        thread_id: id,
        in_reply_to: None,
        from: node(0),
        to: vec![Recipient::Node(peer)],
        subject: "outbound".to_owned(),
        body: "body".to_owned(),
        kind: Kind::Message,
        sender_kind: SenderKind::Human,
        attachments: Vec::new(),
        sent_at: at(0),
        received_at: None,
        signature: Signature::from_bytes([0u8; 64]),
    };
    message
        .sign(&SigningKey::from_bytes(&[9u8; 32]))
        .expect("sign");

    Outbound {
        message,
        recipients: vec![RecipientState::pending(peer)],
    }
}

/// Somewhere to keep what was logged, so a test can read it back.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Sink {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }
}

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Capture at `info`, which is where the daemon runs by default.
///
/// The level is half of what is being tested: a line that needed
/// `HIVEMIND_LOG=debug` to exist would not have helped the incident in #32,
/// where the log was the only thing anybody had.
fn capturing() -> (Sink, tracing::subscriber::DefaultGuard) {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(sink.clone())
        .with_ansi(false)
        .finish();
    (sink, tracing::subscriber::set_default(subscriber))
}

fn book(addrs: &'static [&'static str]) -> impl Fn(NodeId) -> Vec<String> {
    move |_| addrs.iter().map(|a| (*a).to_owned()).collect()
}

/// A peer that takes whatever it is handed.
struct Accepting;

impl Transport for Accepting {
    async fn deliver(
        &self,
        _node: NodeId,
        _addr: &str,
        _message: &Message,
    ) -> Result<(), ClientError> {
        Ok(())
    }
}

/// A peer whose answer is chosen by the port dialled: 8403 refuses the way an
/// unpaired node does, 8409 fails the handshake the way something that is not
/// this peer does, and anything else does not answer at all.
struct ByPort;

impl Transport for ByPort {
    async fn deliver(
        &self,
        _node: NodeId,
        addr: &str,
        _message: &Message,
    ) -> Result<(), ClientError> {
        let addr = addr.to_owned();
        if addr.ends_with(":8403") {
            Err(ClientError::Status {
                addr,
                status: 403,
                detail: "Not paired".to_owned(),
            })
        } else if addr.ends_with(":8409") {
            Err(ClientError::Handshake {
                addr,
                reason: "certificate not pinned".to_owned(),
            })
        } else {
            Err(ClientError::Connect {
                addr,
                source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            })
        }
    }
}

#[tokio::test]
async fn a_refusal_is_a_warning_that_says_when_it_will_try_again() {
    let (log, _guard) = capturing();
    let peer = node(7);
    let mut outbound = outbound_to(peer);

    attempt_all(&mut outbound, book(&["10.0.0.7:8403"]), &ByPort, at(100)).await;

    let text = log.text();
    assert!(
        text.contains("WARN") && text.contains("403"),
        "a refusal is somebody's to resolve, and the line says what was said: {text}"
    );
    assert!(
        text.contains(&peer.short()) && text.contains("10.0.0.7:8403"),
        "which peer, and which address: {text}"
    );
    assert!(
        text.contains("retry_in=2s"),
        "\"stuck\" and \"being patient\" look identical without it: {text}"
    );
}

#[tokio::test]
async fn a_delivery_says_which_address_worked_and_on_which_attempt() {
    let (log, _guard) = capturing();
    let peer = node(3);
    let mut outbound = outbound_to(peer);

    attempt_all(&mut outbound, book(&["10.0.0.3:8400"]), &Accepting, at(100)).await;

    let text = log.text();
    assert!(
        text.contains("delivered") && text.contains("10.0.0.3:8400"),
        "the good news too, with the address that worked: {text}"
    );
    assert!(
        text.contains("attempt=1"),
        "how patient it had to be, which is the answer to \"is it stuck?\": {text}"
    );
    assert!(
        text.contains(&peer.short()) && !text.contains("hm1:"),
        "short ids, and nothing longer: {text}"
    );
}

#[tokio::test]
async fn one_address_failing_differently_from_the_rest_stands_out() {
    // #29's symptom: something that is not this peer answers on one of the
    // addresses we hold for it, so that one fails the handshake while the others
    // are merely unreachable. Nobody would see it any other way.
    let (log, _guard) = capturing();
    let mut outbound = outbound_to(node(9));
    let addresses = book(&["192.168.1.9:8400", "10.0.0.9:8409"]);

    attempt_all(&mut outbound, addresses, &ByPort, at(100)).await;

    let text = log.text();
    assert!(
        text.contains("WARN") && text.contains("10.0.0.9:8409 stranger"),
        "the odd address out, and how it differed: {text}"
    );
}

#[tokio::test]
async fn a_peer_that_is_simply_off_is_reported_without_alarm() {
    // Every address fails the same way, which is the ordinary case, and a warn
    // here would teach somebody to ignore warns.
    let (log, _guard) = capturing();
    let mut outbound = outbound_to(node(4));
    let addresses = book(&["192.168.1.4:8400", "10.0.0.4:8400"]);

    attempt_all(&mut outbound, addresses, &ByPort, at(100)).await;

    let text = log.text();
    assert!(
        text.contains("retry_in=") && !text.contains("WARN") && !text.contains("ERROR"),
        "it says when it will try again, and it does not cry wolf: {text}"
    );
}
