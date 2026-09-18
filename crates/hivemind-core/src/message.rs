//! The `Message` type and its canonical encoding.
//!
//! A message is signed over a deterministic CBOR encoding of its fields so that
//! a message read back off disk is verifiable independently of the transport
//! that carried it (SPEC §4.1). The encoding is frozen by golden vectors; see
//! `docs/protocol.md`.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::crypto::{Sha256Digest, Signature};
use crate::peer::NodeId;

/// The longest a subject may be, in characters (SPEC §4.1).
pub const SUBJECT_MAX_CHARS: usize = 200;
/// The largest a body may be, in bytes (SPEC §4.1).
pub const BODY_MAX_BYTES: usize = 1024 * 1024;

/// What a message is for.
///
/// `Task` exists now so that the autoreply reserved for v2 (SPEC §12) is an
/// addition rather than a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Ordinary mail.
    Message,
    /// A request for work. v1 delivers it like any other message.
    Task,
    /// Something to be shown, not replied to.
    Notification,
}

impl Kind {
    /// The stable string used in the index and on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Task => "task",
            Self::Notification => "notification",
        }
    }

    /// Parse [`Kind::as_str`].
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "message" => Some(Self::Message),
            "task" => Some(Self::Task),
            "notification" => Some(Self::Notification),
            _ => None,
        }
    }
}

/// Whether a person or an agent wrote this.
///
/// Set by the entrypoint — CLI and web UI mean [`SenderKind::Human`], MCP means
/// [`SenderKind::Agent`] — and never by the caller, so it can be relied on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    /// A person typed this directly.
    Human,
    /// A Claude sent this through the MCP server.
    Agent,
}

impl SenderKind {
    /// The stable string used in the index and on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }

    /// Parse [`SenderKind::as_str`].
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "human" => Some(Self::Human),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

/// Who a message is addressed to.
///
/// [`Recipient::Owner`] and [`Recipient::Everyone`] are expanded to concrete
/// nodes at send time and the expansion is stored, so a peer paired later does
/// not retroactively receive older mail (SPEC §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipient {
    /// One machine.
    Node(NodeId),
    /// Every paired machine carrying this `owner` label.
    Owner(String),
    /// Every paired machine.
    Everyone,
}

/// A file travelling with a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    /// The name to show, and to save the file as. Never a path.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Content address of the blob.
    pub sha256: Sha256Digest,
    /// Media type as declared by the sender.
    pub mime: String,
    /// `true` when the blob ships with the message; `false` when it is fetched
    /// on demand (SPEC §8).
    pub inline: bool,
}

/// One piece of mail.
///
/// The field order here is normative: it is the order the canonical encoding
/// writes, and changing it is a wire-compatibility break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Globally unique and time-sortable.
    pub id: Ulid,
    /// Equal to `id` for the root of a thread.
    pub thread_id: Ulid,
    /// The message this replies to, if any.
    pub in_reply_to: Option<Ulid>,
    /// The node that wrote and signed this.
    pub from: NodeId,
    /// Who it is addressed to.
    pub to: Vec<Recipient>,
    /// At most [`SUBJECT_MAX_CHARS`] characters.
    pub subject: String,
    /// Markdown, at most [`BODY_MAX_BYTES`] bytes.
    pub body: String,
    /// What the message is for.
    pub kind: Kind,
    /// Whether a person or an agent wrote it.
    pub sender_kind: SenderKind,
    /// Files travelling with the message.
    pub attachments: Vec<AttachmentRef>,
    /// When the sender sent it. Millisecond precision (see [`Message::canonical_bytes`]).
    pub sent_at: DateTime<Utc>,
    /// When this node received it. Recipient-local, and **not signed**.
    pub received_at: Option<DateTime<Utc>>,
    /// Ed25519 over [`Message::canonical_bytes`].
    pub signature: Signature,
}

/// Why a message is not acceptable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    /// The subject was longer than [`SUBJECT_MAX_CHARS`].
    #[error("subject is {found} characters, the maximum is {SUBJECT_MAX_CHARS}")]
    SubjectTooLong {
        /// The length that was offered.
        found: usize,
    },
    /// The body was larger than [`BODY_MAX_BYTES`].
    #[error("body is {found} bytes, the maximum is {BODY_MAX_BYTES}")]
    BodyTooLarge {
        /// The size that was offered.
        found: usize,
    },
    /// An attachment name was a path, or could escape the directory it is
    /// written into.
    #[error("attachment name `{name}` is not a plain file name")]
    AttachmentNameNotPlain {
        /// The name that was rejected.
        name: String,
    },
}

/// Why a message could not be encoded or verified.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalError {
    /// The message could not be encoded to CBOR.
    #[error("could not encode the message canonically")]
    Encode,
    /// The signature did not match the message.
    #[error("signature does not match this message")]
    BadSignature,
}

impl Message {
    /// Check the limits and the attachment names (SPEC §4.1, §6.3).
    ///
    /// # Errors
    /// Returns the first violation found.
    pub fn validate(&self) -> Result<(), MessageError> {
        // Characters, not bytes: the limit exists to keep subject lines short
        // enough to read, and a subject in Japanese is not four times as long
        // as one in English.
        let subject_chars = self.subject.chars().count();
        if subject_chars > SUBJECT_MAX_CHARS {
            return Err(MessageError::SubjectTooLong {
                found: subject_chars,
            });
        }

        // Bytes, because this limit exists to bound what we store and ship.
        if self.body.len() > BODY_MAX_BYTES {
            return Err(MessageError::BodyTooLarge {
                found: self.body.len(),
            });
        }

        for attachment in &self.attachments {
            if !is_plain_file_name(&attachment.name) {
                return Err(MessageError::AttachmentNameNotPlain {
                    name: attachment.name.clone(),
                });
            }
        }

        Ok(())
    }

    /// The bytes that are signed.
    ///
    /// Deterministic CBOR: a map whose keys appear in the declaration order of
    /// this struct. `received_at` and `signature` are excluded — see
    /// `docs/decisions/0007-received-at-is-not-signed.md`.
    ///
    /// Sign this message in place with the sending node's key.
    ///
    /// # Errors
    /// Returns [`CanonicalError::Encode`] if the message cannot be encoded.
    pub fn sign(&mut self, key: &SigningKey) -> Result<(), CanonicalError> {
        let bytes = self.canonical_bytes()?;
        self.signature = Signature::from_bytes(key.sign(&bytes).to_bytes());
        Ok(())
    }

    /// Check the signature against a node's public key.
    ///
    /// This reads only the signed fields, so a message that has been received
    /// and stamped still verifies.
    ///
    /// # Errors
    /// Returns [`CanonicalError::BadSignature`] if the signature does not match,
    /// or [`CanonicalError::Encode`] if the message cannot be encoded.
    pub fn verify(&self, key: &VerifyingKey) -> Result<(), CanonicalError> {
        let bytes = self.canonical_bytes()?;
        let signature = ed25519_dalek::Signature::from_bytes(self.signature.as_bytes());
        key.verify(&bytes, &signature)
            .map_err(|_| CanonicalError::BadSignature)
    }

    /// # Errors
    /// Returns [`CanonicalError::Encode`] if CBOR encoding fails.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CanonicalError> {
        // Destructured rather than borrowed field by field: adding a field to
        // `Message` then becomes a compile error here, which forces a decision
        // about whether it is signed instead of silently leaving it out.
        let Self {
            id,
            thread_id,
            in_reply_to,
            from,
            to,
            subject,
            body,
            kind,
            sender_kind,
            attachments,
            sent_at,
            received_at: _,
            signature: _,
        } = self;

        let view = Canonical {
            id,
            thread_id,
            in_reply_to,
            from,
            to,
            subject,
            body,
            kind: *kind,
            sender_kind: *sender_kind,
            attachments,
            sent_at: *sent_at,
        };

        let mut out = Vec::new();
        ciborium::into_writer(&view, &mut out).map_err(|_| CanonicalError::Encode)?;
        Ok(out)
    }
}

/// The subset of `Message` that is signed, in the order it is written.
///
/// Separate from `Message` so that the signed field set is one reviewable list
/// rather than a set of `#[serde(skip)]` attributes scattered through a struct.
#[derive(Serialize)]
struct Canonical<'a> {
    id: &'a Ulid,
    thread_id: &'a Ulid,
    in_reply_to: &'a Option<Ulid>,
    from: &'a NodeId,
    to: &'a [Recipient],
    subject: &'a str,
    body: &'a str,
    kind: Kind,
    sender_kind: SenderKind,
    attachments: &'a [AttachmentRef],
    // Integer milliseconds rather than the RFC 3339 string used on disk: two
    // implementations must agree on these bytes exactly, and "shortest
    // representation that round-trips" is not something to ask of a signature.
    #[serde(with = "chrono::serde::ts_milliseconds")]
    sent_at: DateTime<Utc>,
}

/// Reject anything that is not a plain file name (SPEC §6.3).
///
/// Attachments are written into a directory chosen by the *recipient*, so a
/// name is a name: never a path, never a traversal, never a control character
/// that a terminal or a filesystem might interpret.
fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

/// A message built to match `docs/reference/canonical_reference.py`, the
/// independent encoder the golden vector comes from.
///
/// Lives outside the test module so the store and index tests can build a
/// realistic message without each inventing their own.
#[cfg(test)]
pub(crate) fn fixture() -> Message {
    let id = Ulid::from_parts(1_750_000_000_000, 0x0102_0304_0506_0708_090A);
    Message {
        id,
        thread_id: id,
        in_reply_to: None,
        from: NodeId::from_certificate_der(b"hivemind test certificate"),
        to: vec![
            Recipient::Node(NodeId::from_certificate_der(b"recipient certificate")),
            Recipient::Owner("rafael".to_owned()),
            Recipient::Everyone,
        ],
        subject: "dashboard PR".to_owned(),
        body: "Take a look when you get a chance.".to_owned(),
        kind: Kind::Message,
        sender_kind: SenderKind::Human,
        attachments: vec![AttachmentRef {
            name: "notes.md".to_owned(),
            size: 42,
            sha256: Sha256Digest::of(b"notes"),
            mime: "text/markdown".to_owned(),
            inline: true,
        }],
        sent_at: DateTime::from_timestamp_millis(1_750_000_000_000)
            .expect("fixture timestamp is in range"),
        received_at: None,
        signature: Signature::from_bytes([0u8; 64]),
    }
}

/// A fixed key, so a failure is reproducible rather than one run in a million.
#[cfg(test)]
pub(crate) fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by `canonical_reference.py`. If this test fails, the wire
    /// format changed: that is a compatibility break needing an ADR, not a
    /// vector to update (SPEC §4.1).
    const GOLDEN_HEX: &str = "ab626964781a30314a5854323151303030343130363130353052334747323841697468726561645f6964781a30314a58543231513030303431303631303530523347473238416b696e5f7265706c795f746ff66466726f6d5820b6990bba3a91115598f0c0601df91546b8e6f6aa1bed6fefca220484329b0b6d62746f83a1646e6f6465582015605078640c5f71167c21536d3da78275069478eba3886683734940616f33a5a1656f776e65726672616661656c6865766572796f6e65677375626a6563746c64617368626f61726420505264626f6479782254616b652061206c6f6f6b207768656e20796f75206765742061206368616e63652e646b696e64676d6573736167656b73656e6465725f6b696e646568756d616e6b6174746163686d656e747381a5646e616d65686e6f7465732e6d646473697a65182a667368613235365820ab5aa97074c454a0632057e704220d9a6678fbf773a0a5806fc09b8173b07309646d696d656d746578742f6d61726b646f776e66696e6c696e65f56773656e745f61741b000001977420dc00";

    #[test]
    fn the_fixture_ulid_matches_the_reference_encoder() {
        // Guards the fixture itself: if the ulid crate ever rendered this
        // differently, the golden vector would be comparing two different
        // messages and the test below would fail for the wrong reason.
        assert_eq!(fixture().id.to_string(), "01JXT21Q00041061050R3GG28A");
    }

    #[test]
    fn canonical_encoding_matches_the_reference_vector() {
        let encoded = fixture().canonical_bytes().expect("fixture must encode");
        assert_eq!(hex::encode(encoded), GOLDEN_HEX);
    }

    #[test]
    fn canonical_encoding_is_stable_across_repeated_encodes() {
        let message = fixture();
        let first = message.canonical_bytes().expect("must encode");
        let second = message.canonical_bytes().expect("must encode");
        assert_eq!(first, second);
    }

    #[test]
    fn canonical_encoding_writes_fields_in_declaration_order() {
        let encoded = fixture().canonical_bytes().expect("must encode");
        let value: ciborium::Value = ciborium::from_reader(encoded.as_slice()).expect("valid cbor");
        let map = value.as_map().expect("canonical form is a map").clone();
        let keys: Vec<String> = map
            .iter()
            .map(|(k, _)| k.as_text().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "thread_id",
                "in_reply_to",
                "from",
                "to",
                "subject",
                "body",
                "kind",
                "sender_kind",
                "attachments",
                "sent_at",
            ]
        );
    }

    #[test]
    fn canonical_encoding_omits_the_signature() {
        let mut message = fixture();
        let before = message.canonical_bytes().expect("must encode");
        message.signature = Signature::from_bytes([0xAB; 64]);
        let after = message.canonical_bytes().expect("must encode");
        assert_eq!(before, after, "the signature must not sign itself");
    }

    #[test]
    fn canonical_encoding_omits_received_at() {
        // The recipient stamps received_at after verifying. If it were signed,
        // a stored message could never be verified again (ADR 0007).
        let mut message = fixture();
        let as_sent = message.canonical_bytes().expect("must encode");
        message.received_at = Some(
            DateTime::from_timestamp_millis(1_750_000_999_999).expect("timestamp is in range"),
        );
        let as_stored = message.canonical_bytes().expect("must encode");
        assert_eq!(as_sent, as_stored);
    }

    #[test]
    fn editing_the_body_changes_the_canonical_bytes() {
        let message = fixture();
        let mut edited = message.clone();
        edited.body.push('!');
        assert_ne!(
            message.canonical_bytes().expect("must encode"),
            edited.canonical_bytes().expect("must encode")
        );
    }

    #[test]
    fn a_signed_message_verifies() {
        let mut message = fixture();
        message
            .sign(&test_signing_key())
            .expect("signing must succeed");
        assert!(message.verify(&test_signing_key().verifying_key()).is_ok());
    }

    #[test]
    fn signing_is_deterministic_for_the_same_message_and_key() {
        // Ed25519 signatures are deterministic, so two nodes signing the same
        // bytes agree. This also guards against accidentally mixing randomness
        // into the canonical encoding.
        let mut first = fixture();
        let mut second = fixture();
        first
            .sign(&test_signing_key())
            .expect("signing must succeed");
        second
            .sign(&test_signing_key())
            .expect("signing must succeed");
        assert_eq!(first.signature, second.signature);
    }

    #[test]
    fn verification_fails_after_the_body_is_edited() {
        let mut message = fixture();
        message
            .sign(&test_signing_key())
            .expect("signing must succeed");
        message.body.push_str(" actually, never mind.");
        assert!(matches!(
            message.verify(&test_signing_key().verifying_key()),
            Err(CanonicalError::BadSignature)
        ));
    }

    #[test]
    fn verification_fails_against_a_different_nodes_key() {
        let mut message = fixture();
        message
            .sign(&test_signing_key())
            .expect("signing must succeed");
        let impostor = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        assert!(matches!(
            message.verify(&impostor),
            Err(CanonicalError::BadSignature)
        ));
    }

    #[test]
    fn a_message_still_verifies_after_it_has_been_received_and_stamped() {
        // The whole point of ADR 0007: the recipient stamps received_at on
        // arrival, and the message must stay verifiable on disk afterwards.
        let mut message = fixture();
        message
            .sign(&test_signing_key())
            .expect("signing must succeed");
        message.received_at = Some(
            DateTime::from_timestamp_millis(1_750_000_042_000).expect("timestamp is in range"),
        );
        assert!(message.verify(&test_signing_key().verifying_key()).is_ok());
    }

    #[test]
    fn a_message_round_trips_through_the_json_stored_on_disk() {
        let mut message = fixture();
        message
            .sign(&test_signing_key())
            .expect("signing must succeed");
        let json = serde_json::to_string(&message).expect("must serialise");
        let back: Message = serde_json::from_str(&json).expect("must deserialise");
        assert_eq!(back, message);
    }

    #[test]
    fn a_signed_message_still_verifies_after_a_json_round_trip() {
        // This is the property ADR 0002 and ADR 0007 are both for: files are
        // the source of truth, and what is read back off disk is verifiable.
        let mut message = fixture();
        message
            .sign(&test_signing_key())
            .expect("signing must succeed");
        let json = serde_json::to_string(&message).expect("must serialise");
        let back: Message = serde_json::from_str(&json).expect("must deserialise");
        assert!(back.verify(&test_signing_key().verifying_key()).is_ok());
    }

    #[test]
    fn json_on_disk_renders_byte_fields_as_hex_not_arrays_of_numbers() {
        // Users are invited to read ~/.hivemind/mail with ordinary tools
        // (ADR 0002). A JSON array of 32 integers is not readable.
        let json = serde_json::to_value(fixture()).expect("must serialise");
        assert_eq!(
            json["from"],
            serde_json::json!("b6990bba3a91115598f0c0601df91546b8e6f6aa1bed6fefca220484329b0b6d")
        );
        assert_eq!(json["sent_at"], serde_json::json!("2025-06-15T15:06:40Z"));
    }

    #[test]
    fn the_canonical_encoding_is_not_the_on_disk_encoding() {
        // Same message, two representations on purpose: compact and unambiguous
        // for signing, readable for storage.
        let message = fixture();
        let canonical = message.canonical_bytes().expect("must encode");
        let json = serde_json::to_vec(&message).expect("must serialise");
        assert_ne!(canonical, json);
    }

    #[test]
    fn a_message_within_the_limits_validates() {
        assert_eq!(fixture().validate(), Ok(()));
    }

    #[test]
    fn a_subject_over_two_hundred_characters_is_rejected() {
        let mut message = fixture();
        message.subject = "s".repeat(SUBJECT_MAX_CHARS + 1);
        assert_eq!(
            message.validate(),
            Err(MessageError::SubjectTooLong {
                found: SUBJECT_MAX_CHARS + 1
            })
        );
    }

    #[test]
    fn a_subject_of_exactly_two_hundred_characters_is_accepted() {
        let mut message = fixture();
        message.subject = "s".repeat(SUBJECT_MAX_CHARS);
        assert_eq!(message.validate(), Ok(()));
    }

    #[test]
    fn subject_length_is_counted_in_characters_not_bytes() {
        // 200 emoji are 800 bytes but 200 characters, and the spec says
        // characters. A subject should not be rejected for being in Japanese.
        let mut message = fixture();
        message.subject = "🐝".repeat(SUBJECT_MAX_CHARS);
        assert_eq!(message.validate(), Ok(()));
    }

    #[test]
    fn a_body_over_one_mebibyte_is_rejected() {
        let mut message = fixture();
        message.body = "b".repeat(BODY_MAX_BYTES + 1);
        assert_eq!(
            message.validate(),
            Err(MessageError::BodyTooLarge {
                found: BODY_MAX_BYTES + 1
            })
        );
    }

    #[test]
    fn an_attachment_name_containing_a_path_separator_is_rejected() {
        let mut message = fixture();
        message.attachments[0].name = "subdir/notes.md".to_owned();
        assert_eq!(
            message.validate(),
            Err(MessageError::AttachmentNameNotPlain {
                name: "subdir/notes.md".to_owned()
            })
        );
    }

    #[test]
    fn an_attachment_name_traversing_to_a_parent_directory_is_rejected() {
        for name in ["..", "../../.ssh/authorized_keys", "..\\windows"] {
            let mut message = fixture();
            message.attachments[0].name = name.to_owned();
            assert_eq!(
                message.validate(),
                Err(MessageError::AttachmentNameNotPlain {
                    name: name.to_owned()
                }),
                "{name} should have been rejected"
            );
        }
    }

    #[test]
    fn an_absolute_attachment_name_is_rejected() {
        let mut message = fixture();
        message.attachments[0].name = "/etc/passwd".to_owned();
        assert_eq!(
            message.validate(),
            Err(MessageError::AttachmentNameNotPlain {
                name: "/etc/passwd".to_owned()
            })
        );
    }

    #[test]
    fn an_empty_attachment_name_is_rejected() {
        let mut message = fixture();
        message.attachments[0].name = String::new();
        assert_eq!(
            message.validate(),
            Err(MessageError::AttachmentNameNotPlain {
                name: String::new()
            })
        );
    }

    #[test]
    fn an_attachment_name_containing_a_nul_byte_is_rejected() {
        let mut message = fixture();
        message.attachments[0].name = "notes\0.md".to_owned();
        assert_eq!(
            message.validate(),
            Err(MessageError::AttachmentNameNotPlain {
                name: "notes\0.md".to_owned()
            })
        );
    }
}
