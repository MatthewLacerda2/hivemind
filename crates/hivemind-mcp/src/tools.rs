//! The seven tools and the two resources (SPEC §9.1).
//!
//! Tool descriptions are part of the product: they are what tells Claude that
//! `sender_kind: human` means a person typed the message, and that `to` accepts
//! a node name, an owner name or `everyone`.

use hivemind_api::service::{Draft, ServiceError};
use hivemind_core::index::Query;
use hivemind_core::message::{Kind, Recipient, SenderKind};
use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::server::HivemindMcp;

/// Turn a service failure into something Claude can act on.
///
/// The distinction that matters is "you asked for something that is not there"
/// versus "this node is broken": the first is worth retrying differently, the
/// second is not.
fn mcp_error(error: &ServiceError) -> McpError {
    match error {
        ServiceError::NoSuchMessage { id } => {
            McpError::invalid_params(format!("no message with id {id}"), None)
        }
        ServiceError::NoRecipients => McpError::invalid_params(
            "a message needs at least one recipient: a node id, an owner name, or `everyone`",
            None,
        ),
        ServiceError::Invalid(inner) => McpError::invalid_params(inner.to_string(), None),
        other => McpError::internal_error(other.to_string(), None),
    }
}

fn parse_id(raw: &str) -> Result<Ulid, McpError> {
    raw.parse()
        .map_err(|_| McpError::invalid_params(format!("`{raw}` is not a message id"), None))
}

/// A recipient string is a node id, `everyone`, or an owner name.
fn parse_recipient(raw: &str) -> Recipient {
    if raw.eq_ignore_ascii_case("everyone") {
        return Recipient::Everyone;
    }
    raw.parse()
        .map_or_else(|_| Recipient::Owner(raw.to_owned()), Recipient::Node)
}

// ------------------------------------------------------------ parameters ---

/// Arguments for `send`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendParams {
    /// Who to send to. Each entry is a node name, an owner name (which reaches
    /// every machine that person runs), or `everyone`.
    pub to: Vec<String>,
    /// A one-line subject, at most 200 characters.
    pub subject: String,
    /// The message body, as markdown.
    pub body: String,
    /// `message` (the default), `task`, or `notification`.
    pub kind: Option<String>,
    /// Absolute paths to local files to send with the message. They are copied
    /// into hivemind's own storage immediately, so the originals can be moved
    /// or deleted afterwards.
    pub attachments: Option<Vec<String>>,
}

/// Turn the paths an agent supplied into real ones.
///
/// No validation here beyond the shape: whether a path exists, is readable and
/// is within the size limit is the blob store's answer to give, with a message
/// the agent can act on.
fn local_paths(paths: Option<Vec<String>>) -> Vec<std::path::PathBuf> {
    paths
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect()
}

/// Arguments for `inbox`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InboxParams {
    /// Only show mail that has not been read yet.
    pub unread_only: Option<bool>,
    /// How many messages to return. Defaults to 20.
    pub limit: Option<usize>,
    /// Only show mail from this node id.
    pub from: Option<String>,
}

/// Arguments for a tool that takes only a message id.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MessageIdParams {
    /// The message id, as shown by `inbox`.
    pub id: String,
}

/// Arguments for `reply`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReplyParams {
    /// The id of the message being replied to.
    pub id: String,
    /// The reply body, as markdown.
    pub body: String,
    /// Absolute paths to local files to send with the message. They are copied
    /// into hivemind's own storage immediately, so the originals can be moved
    /// or deleted afterwards.
    pub attachments: Option<Vec<String>>,
}

/// Arguments for `broadcast`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BroadcastParams {
    /// A one-line subject, at most 200 characters.
    pub subject: String,
    /// The message body, as markdown.
    pub body: String,
    /// `message` (the default), `task`, or `notification`.
    pub kind: Option<String>,
}

/// Arguments for `download_attachment`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DownloadParams {
    /// The id of the message the attachment belongs to.
    pub id: String,
    /// The attachment's SHA-256, as shown by `read`.
    pub sha: String,
}

// --------------------------------------------------------------- results ---

/// What `send`, `reply` and `broadcast` return.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Sent {
    /// The new message's id.
    pub id: String,
    /// The thread it belongs to.
    pub thread_id: String,
}

/// One row of the inbox.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InboxItem {
    /// The message id; pass it to `read`.
    pub id: String,
    /// The node that sent it.
    pub from: String,
    /// Its subject.
    pub subject: String,
    /// `message`, `task` or `notification`.
    pub kind: String,
    /// `human` if a person typed this, `agent` if another Claude sent it.
    pub sender_kind: String,
    /// When it was sent, RFC 3339.
    pub sent_at: String,
    /// Whether it is still unread.
    pub unread: bool,
    /// The names of any attachments.
    pub attachment_names: Vec<String>,
}

/// A whole message, as `read` returns it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FullMessage {
    /// The message id.
    pub id: String,
    /// The thread it belongs to; pass it to `reply` to stay in the conversation.
    pub thread_id: String,
    /// The node that sent it.
    pub from: String,
    /// Its subject.
    pub subject: String,
    /// Its body, as markdown.
    pub body: String,
    /// `message`, `task` or `notification`.
    pub kind: String,
    /// `human` if a person typed this, `agent` if another Claude sent it.
    pub sender_kind: String,
    /// When it was sent, RFC 3339.
    pub sent_at: String,
    /// Attachments, each with a local filesystem path once downloaded.
    pub attachments: Vec<AttachmentInfo>,
}

/// An attachment, and where to find it on disk.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AttachmentInfo {
    /// The file name the sender chose.
    pub name: String,
    /// Its SHA-256; pass it to `download_attachment`.
    pub sha: String,
    /// Its size in bytes.
    pub size: u64,
    /// Where it is on this machine, if it has been fetched.
    pub path: Option<String>,
}

/// A peer, as `list_peers` returns it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PeerInfo {
    /// The peer's node id.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// The human who owns it, if they said.
    pub owner: Option<String>,
    /// Whether it answered recently.
    pub online: bool,
    /// When it was last seen, RFC 3339.
    pub last_seen: Option<String>,
}

/// What `download_attachment` returns.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Downloaded {
    /// Where the file is on this machine.
    pub path: String,
}

// ----------------------------------------------------------------- tools ---

#[tool_router(vis = "pub(crate)")]
impl HivemindMcp {
    #[tool(
        name = "inbox",
        description = "List mail that has arrived on this machine, newest first. \
                       Check this when the user asks about messages, or when you want to \
                       know whether a teammate or another Claude has sent anything. \
                       `sender_kind` is `human` when a person typed the message directly \
                       and `agent` when another Claude sent it."
    )]
    async fn inbox(
        &self,
        Parameters(params): Parameters<InboxParams>,
    ) -> Result<Json<Vec<InboxItem>>, McpError> {
        let query = Query {
            mailbox: Some(hivemind_core::store::Mailbox::New),
            unread_only: params.unread_only.unwrap_or(false),
            from: params.from.as_deref().and_then(|f| f.parse().ok()),
            limit: Some(params.limit.unwrap_or(20)),
            ..Query::default()
        };

        let summaries = self.service.list(&query).map_err(|e| mcp_error(&e))?;
        Ok(Json(
            summaries
                .into_iter()
                .map(|s| InboxItem {
                    id: s.id.to_string(),
                    from: s.from.to_string(),
                    subject: s.subject,
                    kind: s.kind.as_str().to_owned(),
                    sender_kind: s.sender_kind.as_str().to_owned(),
                    sent_at: s.sent_at.to_rfc3339(),
                    unread: s.mailbox.is_unread(),
                    attachment_names: s.attachment_names,
                })
                .collect(),
        ))
    }

    #[tool(
        name = "read",
        description = "Read one message in full and mark it as read. Use the id from \
                       `inbox`. The reply you send back should go through `reply` so it \
                       stays in the same thread."
    )]
    async fn read(
        &self,
        Parameters(params): Parameters<MessageIdParams>,
    ) -> Result<Json<FullMessage>, McpError> {
        let id = parse_id(&params.id)?;
        let (_, message) = self.service.get(id).map_err(|e| mcp_error(&e))?;
        // Reading is what marks it read, so a Claude that looked at its mail
        // does not see it again on the next turn.
        self.service.mark_read(id).map_err(|e| mcp_error(&e))?;

        Ok(Json(FullMessage {
            id: message.id.to_string(),
            thread_id: message.thread_id.to_string(),
            from: message.from.to_string(),
            subject: message.subject,
            body: message.body,
            kind: message.kind.as_str().to_owned(),
            sender_kind: message.sender_kind.as_str().to_owned(),
            sent_at: message.sent_at.to_rfc3339(),
            attachments: message
                .attachments
                .iter()
                .map(|a| AttachmentInfo {
                    name: a.name.clone(),
                    sha: a.sha256.to_string(),
                    size: a.size,
                    // A path only when the bytes are actually here. Handing
                    // back a path to a file that does not exist would send a
                    // Claude off to open nothing (SPEC §9.1).
                    path: self.service.blobs().has(&a.sha256).then(|| {
                        self.service
                            .blobs()
                            .path_of(&a.sha256)
                            .to_string_lossy()
                            .into_owned()
                    }),
                })
                .collect(),
        }))
    }

    #[tool(
        name = "send",
        description = "Send a message to one or more recipients. Each recipient is a node \
                       name (one machine), an owner name (every machine that person runs), \
                       or `everyone`. Sending never blocks on the network: the message is \
                       queued and delivered when the recipient is reachable, which may be \
                       days later if their laptop is closed."
    )]
    async fn send(
        &self,
        Parameters(params): Parameters<SendParams>,
    ) -> Result<Json<Sent>, McpError> {
        let draft = Draft {
            to: params.to.iter().map(|s| parse_recipient(s)).collect(),
            subject: params.subject,
            body: params.body,
            kind: params
                .kind
                .as_deref()
                .and_then(Kind::from_str_opt)
                .unwrap_or(Kind::Message),
            in_reply_to: None,
            attachments: local_paths(params.attachments),
        };

        // MCP means an agent, always. The caller does not get to say otherwise
        // (SPEC §4.1).
        let message = self
            .service
            .send(draft, SenderKind::Agent)
            .map_err(|e| mcp_error(&e))?;

        Ok(Json(Sent {
            id: message.id.to_string(),
            thread_id: message.thread_id.to_string(),
        }))
    }

    #[tool(
        name = "reply",
        description = "Reply to a message, keeping it in the same thread. Prefer this over \
                       `send` when responding to something in the inbox, so the \
                       conversation stays readable to the person on the other end."
    )]
    async fn reply(
        &self,
        Parameters(params): Parameters<ReplyParams>,
    ) -> Result<Json<Sent>, McpError> {
        let id = parse_id(&params.id)?;
        let message = self
            .service
            .reply(
                id,
                params.body,
                local_paths(params.attachments),
                SenderKind::Agent,
            )
            .map_err(|e| mcp_error(&e))?;

        Ok(Json(Sent {
            id: message.id.to_string(),
            thread_id: message.thread_id.to_string(),
        }))
    }

    #[tool(
        name = "broadcast",
        description = "Send one message to every paired machine. Use sparingly — it reaches \
                       every person on the network, not just the one you were talking to."
    )]
    async fn broadcast(
        &self,
        Parameters(params): Parameters<BroadcastParams>,
    ) -> Result<Json<Sent>, McpError> {
        let draft = Draft {
            to: vec![Recipient::Everyone],
            subject: params.subject,
            body: params.body,
            kind: params
                .kind
                .as_deref()
                .and_then(Kind::from_str_opt)
                .unwrap_or(Kind::Message),
            in_reply_to: None,
            // SPEC §9.1: broadcast takes no attachments. Sending a large file
            // to everybody is rarely what was meant, and `send` is right there.
            attachments: Vec::new(),
        };

        let message = self
            .service
            .send(draft, SenderKind::Agent)
            .map_err(|e| mcp_error(&e))?;

        Ok(Json(Sent {
            id: message.id.to_string(),
            thread_id: message.thread_id.to_string(),
        }))
    }

    #[tool(
        name = "list_peers",
        description = "List the machines this node is paired with. An empty list means \
                       nothing is paired yet, so `send` can only reach this machine."
    )]
    async fn list_peers(&self) -> Result<Json<Vec<PeerInfo>>, McpError> {
        // Pairing and the peer book arrive in M3 (SPEC §14). Until then this is
        // truthfully empty rather than absent: the tool surface is the contract.
        Ok(Json(Vec::new()))
    }

    #[tool(
        name = "download_attachment",
        description = "Fetch an attachment and return its path on this machine, so you can \
                       read it with your normal file tools. Use the `sha` from `read`."
    )]
    async fn download_attachment(
        &self,
        Parameters(params): Parameters<DownloadParams>,
    ) -> Result<Json<Downloaded>, McpError> {
        let id = parse_id(&params.id)?;
        let (_, message) = self.service.get(id).map_err(|e| mcp_error(&e))?;

        let found = message
            .attachments
            .iter()
            .find(|a| a.sha256.to_string() == params.sha);

        match found {
            Some(attachment) => {
                // Blocks until it is here. An agent asked for the file, not
                // for a progress report, and it has nothing to do until the
                // bytes exist.
                let path = self
                    .service
                    .fetch_attachment(id, attachment.sha256)
                    .await
                    .map_err(|e| mcp_error(&e))?;
                Ok(Json(Downloaded {
                    path: path.to_string_lossy().into_owned(),
                }))
            }
            None => Err(McpError::invalid_params(
                format!("message {id} has no attachment with sha {}", params.sha),
                None,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everyone_is_recognised_whatever_its_case() {
        assert_eq!(parse_recipient("everyone"), Recipient::Everyone);
        assert_eq!(parse_recipient("EVERYONE"), Recipient::Everyone);
    }

    #[test]
    fn a_recipient_that_is_not_a_node_id_is_read_as_an_owner_name() {
        assert_eq!(
            parse_recipient("rafael"),
            Recipient::Owner("rafael".to_owned())
        );
    }

    #[test]
    fn a_bad_message_id_is_the_callers_fault_not_an_internal_error() {
        // Claude can recover from "you passed a bad id"; it cannot recover from
        // "this node is broken". The distinction has to survive the mapping.
        let error = parse_id("not-a-ulid").expect_err("should reject");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn a_missing_message_is_the_callers_fault_too() {
        let error = mcp_error(&ServiceError::NoSuchMessage { id: Ulid::nil() });
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn a_broken_index_is_not_blamed_on_the_caller() {
        let error = mcp_error(&ServiceError::Unavailable);
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }
}
