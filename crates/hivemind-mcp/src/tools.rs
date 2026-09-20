//! The seven tools and the two resources (SPEC §9.1).
//!
//! Tool descriptions are part of the product: they are what tells Claude that
//! `sender_kind: human` means a person typed the message, and that `to` accepts
//! a node name, an owner name or `everyone`.

use hivemind_api::service::{Draft, ServiceError};
use hivemind_core::index::Query;
use hivemind_core::message::{Kind, Recipient, SenderKind};
use hivemind_core::store::Mailbox;
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
        // Both of these are the caller's to fix, and both say how: one that
        // nothing matched, one that several did and which. Falling through to
        // `internal_error` would tell an agent the node was broken, which is
        // the one thing it cannot recover from (#27).
        ServiceError::NoSuchMessageTail { .. } | ServiceError::AmbiguousMessage { .. } => {
            McpError::invalid_params(error.to_string(), None)
        }
        ServiceError::NoRecipients => McpError::invalid_params(
            "a message needs at least one recipient: a node id, an owner name, or `everyone`",
            None,
        ),
        ServiceError::Invalid(inner) => McpError::invalid_params(inner.to_string(), None),
        other => McpError::internal_error(other.to_string(), None),
    }
}

impl HivemindMcp {
    /// Turn what an agent passed into a message id.
    ///
    /// Through the service, so `read` accepts exactly what `inbox` printed —
    /// the same resolution the loopback API does, because there is one
    /// implementation of every operation (SPEC §3).
    fn resolve(&self, raw: &str) -> Result<Ulid, McpError> {
        self.service.resolve_message(raw).map_err(|e| mcp_error(&e))
    }
}

// ------------------------------------------------------------ parameters ---

/// Arguments for `send`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendParams {
    /// Who to send to. Each entry is a node's short id as `list_peers` shows
    /// it, its full `hm1:` fingerprint, an owner name (which reaches every
    /// machine that person runs), or `everyone`.
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
    /// Which box to list, `new` by default. `new` is mail that arrived here
    /// and has not been read, `cur` mail that arrived and has been read,
    /// `out` mail sent from this machine that has **not been delivered yet**
    /// — the recipient's machine is off, and hivemind keeps retrying until it
    /// is not — and `sent` mail that reached every recipient.
    pub r#box: Option<String>,
}

impl InboxParams {
    /// Which box to list, and `new` when the caller did not say.
    ///
    /// `new` rather than everything that arrived, because `read` marks a
    /// message read precisely so that a Claude does not meet it again on the
    /// next turn.
    ///
    /// A name that is not one of the four is refused rather than ignored. It
    /// used to be neither: `box` was not a parameter at all, so asking for one
    /// got the default listing back and nothing said the question had not been
    /// answered (#26, and #28 one door along).
    fn mailbox(&self) -> Result<Mailbox, McpError> {
        let Some(raw) = self.r#box.as_deref() else {
            return Ok(Mailbox::New);
        };
        Mailbox::from_str_opt(raw).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "`{raw}` is not a box. There are four: new (arrived, unread), \
                     cur (arrived, read), out (sent from here, not delivered yet) \
                     and sent (delivered to everyone)."
                ),
                None,
            )
        })
    }
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
    /// What each open Claude Code session on that machine is working on
    /// (SPEC §9.3). Empty when there are none, or none reported.
    pub sessions: Vec<String>,
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
        description = "List mail on this machine, newest first: by default what has \
                       arrived and not been read yet. Check this when the user asks about \
                       messages, or when you want to know whether a teammate or another \
                       Claude has sent anything. `sender_kind` is `human` when a person \
                       typed the message directly and `agent` when another Claude sent it. \
                       Pass `box` to look at the other three: `out` is what this machine \
                       has sent that has not been delivered yet, because the recipient's \
                       machine is off — ask for it when you have sent something and want \
                       to know whether it arrived — and `sent` is what reached every \
                       recipient. `cur` is mail that arrived here and has been read."
    )]
    async fn inbox(
        &self,
        Parameters(params): Parameters<InboxParams>,
    ) -> Result<Json<Vec<InboxItem>>, McpError> {
        let query = Query {
            mailbox: Some(params.mailbox()?),
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
        let id = self.resolve(&params.id)?;
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
            // Through the service, which knows the address book. An agent
            // reading `list_peers` is handed short ids, and sending to one
            // used to deliver to nobody and report success (#19).
            to: params
                .to
                .iter()
                .map(|typed| self.service.parse_recipient(typed))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| mcp_error(&e))?,
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
        let id = self.resolve(&params.id)?;
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
        description = "List the machines in this node's group: who they are, whether each \
                       is up right now, and what each open Claude Code session on it is \
                       working on. A session tells you where somebody is working, not who \
                       to address — mail goes to the machine and any session there can \
                       read it. An empty list means this machine is in no group yet, or \
                       has met nobody in it, so `send` can only reach this machine."
    )]
    async fn list_peers(&self) -> Result<Json<Vec<PeerInfo>>, McpError> {
        // Only members, not nodes merely seen: this answers "who can I write
        // to", and a node outside the group is not one of them (SPEC §6.2).
        let peers = self.service.paired_peers().map_err(|e| mcp_error(&e))?;
        Ok(Json(
            peers
                .into_iter()
                .map(|peer| {
                    let presence = self.service.presence_of(peer.id);
                    PeerInfo {
                        id: peer.id.short(),
                        name: peer.name,
                        owner: peer.owner,
                        online: presence.is_some(),
                        last_seen: peer.last_seen.map(|at| at.to_rfc3339()),
                        sessions: presence
                            .map(|p| p.sessions.into_iter().map(|s| s.label).collect())
                            .unwrap_or_default(),
                    }
                })
                .collect(),
        ))
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
        let id = self.resolve(&params.id)?;
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
    fn a_bad_message_id_is_the_callers_fault_not_an_internal_error() {
        // Claude can recover from "you passed a bad id"; it cannot recover from
        // "this node is broken". The distinction has to survive the mapping.
        //
        // Through the mapping rather than through a parse, because resolving
        // an id now needs an index: what is being checked is that a caller's
        // mistake stays the caller's (#27).
        let error = mcp_error(&ServiceError::NoSuchMessageTail {
            typed: "not-a-ulid".to_owned(),
        });
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn an_ambiguous_id_is_the_callers_fault_and_names_what_it_matched() {
        // An agent handed "matches several" can pick one. An agent handed
        // "internal error" can only give up.
        let error = mcp_error(&ServiceError::AmbiguousMessage {
            typed: "AB".to_owned(),
            candidates: vec![Ulid::nil(), Ulid::from_parts(1, 2)],
        });
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("AB"), "{}", error.message);
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
