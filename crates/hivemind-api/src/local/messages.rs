//! The shapes the loopback API speaks about mail in (SPEC §7.1).
//!
//! Split out of `local.rs` because they are one concern — what a message looks
//! like on the way in and on the way out — and because that file had reached
//! its size limit; `local/threads.rs` and `local/group.rs` were already here.
//! The handlers stay in `local.rs` with the router they are wired into.

use chrono::{DateTime, Utc};
use hivemind_core::index::Summary;
use hivemind_core::message::Message;
use hivemind_core::store::Mailbox;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::problem::Problem;

/// One row of a listing (SPEC §9.1).
#[derive(Debug, Serialize, ToSchema)]
pub struct MessageSummary {
    /// The message id.
    pub id: String,
    /// The thread it belongs to.
    pub thread_id: String,
    /// Who sent it.
    pub from: String,
    /// Its subject.
    pub subject: String,
    /// `message`, `task` or `notification`.
    pub kind: String,
    /// `human` if a person typed it, `agent` if a Claude sent it.
    pub sender_kind: String,
    /// When it was sent.
    pub sent_at: DateTime<Utc>,
    /// Which mailbox it is in.
    pub mailbox: String,
    /// Whether it is still unread.
    pub unread: bool,
    /// The names of its attachments.
    pub attachment_names: Vec<String>,
}

impl From<Summary> for MessageSummary {
    fn from(s: Summary) -> Self {
        // Read the derived flag before the struct is torn apart.
        let unread = s.is_unread();
        Self {
            id: s.id.to_string(),
            thread_id: s.thread_id.to_string(),
            from: s.from.to_string(),
            subject: s.subject,
            kind: s.kind.as_str().to_owned(),
            sender_kind: s.sender_kind.as_str().to_owned(),
            sent_at: s.sent_at,
            mailbox: s.mailbox.as_str().to_owned(),
            unread,
            attachment_names: s.attachment_names,
        }
    }
}

/// A whole message.
#[derive(Debug, Serialize, ToSchema)]
pub struct MessageBody {
    /// The message id.
    pub id: String,
    /// The thread it belongs to.
    pub thread_id: String,
    /// What it replies to, if anything.
    pub in_reply_to: Option<String>,
    /// Who sent it.
    pub from: String,
    /// Its subject.
    pub subject: String,
    /// Its body, as markdown.
    pub body: String,
    /// `message`, `task` or `notification`.
    pub kind: String,
    /// `human` if a person typed it, `agent` if a Claude sent it.
    pub sender_kind: String,
    /// When it was sent.
    pub sent_at: DateTime<Utc>,
    /// When this node received it, if it did.
    pub received_at: Option<DateTime<Utc>>,
    /// Which mailbox it is in.
    pub mailbox: String,
    /// The files that came with it.
    pub attachments: Vec<Attachment>,
}

/// One attachment, as the local API describes it (SPEC §7.1).
#[derive(Debug, Serialize, ToSchema)]
pub struct Attachment {
    /// The name to save it as. Checked on arrival, never a path (SPEC §6.3).
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its content address, and the `sha` in the attachment URL.
    pub sha256: String,
    /// What the sender says it is. Advisory (SPEC §4.1).
    pub mime: String,
    /// Whether it travelled with the message or is fetched on demand.
    pub inline: bool,
    /// Whether the bytes are already on this machine. `false` means the first
    /// read of it will go to the sender.
    pub cached: bool,
}

impl MessageBody {
    pub(super) fn new(
        mailbox: Mailbox,
        message: Message,
        blobs: &hivemind_core::blobs::BlobStore,
    ) -> Self {
        Self {
            id: message.id.to_string(),
            thread_id: message.thread_id.to_string(),
            in_reply_to: message.in_reply_to.map(|u| u.to_string()),
            from: message.from.to_string(),
            subject: message.subject,
            body: message.body,
            kind: message.kind.as_str().to_owned(),
            sender_kind: message.sender_kind.as_str().to_owned(),
            sent_at: message.sent_at,
            received_at: message.received_at,
            mailbox: mailbox.as_str().to_owned(),
            attachments: message
                .attachments
                .into_iter()
                .map(|a| Attachment {
                    // Whether the bytes are here is asked now rather than
                    // stored: a lazy attachment becomes cached the moment
                    // somebody reads it.
                    cached: blobs.has(&a.sha256),
                    sha256: a.sha256.to_hex(),
                    name: a.name,
                    size: a.size,
                    mime: a.mime,
                    inline: a.inline,
                })
                .collect(),
        }
    }
}

/// What to send.
///
/// There is deliberately no `sender_kind`: it is decided by which entrypoint
/// the request arrived through, and a caller offering one is ignored
/// (SPEC §4.1).
#[derive(Debug, Deserialize, ToSchema)]
pub struct SendRequest {
    /// Recipients: a node id, an owner name, or `everyone`.
    #[schema(example = json!(["everyone"]))]
    pub to: Vec<String>,
    /// The subject, at most 200 characters.
    pub subject: String,
    /// The body, as markdown.
    pub body: String,
    /// `message`, `task` or `notification`. Defaults to `message`.
    pub kind: Option<String>,
    /// Absolute paths to local files to send with it. The API is loopback
    /// only, so these are paths on this machine (SPEC §7.1).
    pub attachments: Option<Vec<String>>,
}

/// What to say in a reply.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReplyRequest {
    /// The body, as markdown.
    pub body: String,
    /// Absolute paths to local files to send with it. The API is loopback
    /// only, so these are paths on this machine (SPEC §7.1).
    pub attachments: Option<Vec<String>>,
}

/// What a send returns (SPEC §8: accepted, not delivered).
#[derive(Debug, Serialize, ToSchema)]
pub struct Accepted {
    /// The new message's id.
    pub id: String,
    /// The thread it belongs to.
    pub thread_id: String,
    /// An identical message sent in the last two minutes, if there was one
    /// (#33). Absent when there was not. The message was still queued: this is
    /// a notice for whoever pressed send, not a refusal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
}

impl From<crate::service::Queued> for Accepted {
    fn from(queued: crate::service::Queued) -> Self {
        Self {
            id: queued.message.id.to_string(),
            thread_id: queued.message.thread_id.to_string(),
            duplicate_of: queued.duplicate_of.map(|id| id.to_string()),
        }
    }
}

/// Listing filters (SPEC §7.1).
///
/// `deny_unknown_fields` is the point of #28. `?mailbox=sent` instead of
/// `?box=sent` used to be accepted and ignored, so two different questions
/// gave the same answer and the Claude that typed it concluded the `out`
/// mailbox was broken. It was not; what was broken was the API not saying the
/// question made no sense.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[serde(default, deny_unknown_fields)]
pub struct ListParams {
    /// Restrict to one mailbox: `new`, `cur`, `out` or `sent`.
    pub r#box: Option<String>,
    /// Restrict to one thread.
    pub thread: Option<String>,
    /// Restrict to one sender, by node id.
    pub from: Option<String>,
    /// Only unread messages.
    pub unread: Option<bool>,
    /// Full-text search over subject and body.
    pub q: Option<String>,
    /// How many to return.
    pub limit: Option<usize>,
    /// Continue after this row, from a previous page (SPEC §7.1).
    ///
    /// Build it from the last summary already received: `<sent_at in
    /// milliseconds>:<id>`. Keyset rather than offset, so a message arriving
    /// while somebody pages through their inbox cannot make a row appear
    /// twice or not at all.
    pub cursor: Option<String>,
}

/// Parse a filter that was given, or leave it unset.
///
/// The difference that mattered in #28: absent means "do not filter", but
/// **present and unparseable means the caller asked something that makes no
/// sense**, and answering it with an unfiltered list is a wrong answer that
/// looks like a right one.
pub(super) fn parse_filter<T>(
    raw: Option<&str>,
    name: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>, Problem> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match parse(raw) {
        Some(value) => Ok(Some(value)),
        None => Err(Problem::new(
            crate::problem::ProblemType::InvalidQuery,
            match name {
                "box" => format!("`{raw}` is not a mailbox; try new, cur, out or sent"),
                other => format!("`{raw}` is not a valid `{other}`"),
            },
        )),
    }
}

/// Turn the paths a local caller supplied into real ones.
///
/// Nothing is validated here: whether a path exists, is readable and fits
/// under the size limit is the blob store's answer to give, with a message the
/// caller can act on.
pub(super) fn local_paths(paths: Option<Vec<String>>) -> Vec<std::path::PathBuf> {
    paths
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect()
}
