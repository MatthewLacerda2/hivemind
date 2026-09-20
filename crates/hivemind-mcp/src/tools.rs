//! The eight tools and the two resources (SPEC §9.1).
//!
//! Tool descriptions are part of the product: they are what tells Claude that
//! `sender_kind: human` means a person typed the message, and that `to` accepts
//! a node name, an owner name or `everyone`.

use hivemind_api::service::{Draft, Queued, ServiceError};
use hivemind_core::index::Query;
use hivemind_core::message::{Kind, Recipient, SenderKind};
use hivemind_core::peer::NodeId;
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
        //
        // `NoSuchPeer` is the same judgement one door along: an agent handed a
        // node it does not know can pick another, and an agent told the node is
        // broken can only give up.
        ServiceError::NoSuchMessageTail { .. }
        | ServiceError::AmbiguousMessage { .. }
        | ServiceError::NoSuchPeer { .. } => McpError::invalid_params(error.to_string(), None),
        ServiceError::NoRecipients => McpError::invalid_params(
            "a message needs at least one recipient: a node id, an owner name, or `everyone`",
            None,
        ),
        ServiceError::Invalid(inner) => McpError::invalid_params(inner.to_string(), None),
        other => McpError::internal_error(other.to_string(), None),
    }
}

impl HivemindMcp {
    /// One message as `read` and `thread` both report it.
    ///
    /// `others_in_thread` is passed in rather than counted here: `thread`
    /// already knows the answer for every message it is about to return, and
    /// counting it again per message would be a query each.
    fn full(
        &self,
        message: hivemind_core::message::Message,
        others_in_thread: usize,
    ) -> FullMessage {
        FullMessage {
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
            others_in_thread,
        }
    }

    /// Turn what an agent passed into a message id.
    ///
    /// Through the service, so `read` accepts exactly what `inbox` printed —
    /// the same resolution the loopback API does, because there is one
    /// implementation of every operation (SPEC §3).
    fn resolve(&self, raw: &str) -> Result<Ulid, McpError> {
        self.service.resolve_message(raw).map_err(|e| mcp_error(&e))
    }

    /// The index query one MCP listing asks for.
    ///
    /// One construction for the `inbox` tool and for the `hivemind://inbox`
    /// resource, which asked the same question in two places until #102 — and
    /// every filter in both of them could be deleted with the suite still
    /// green. One question asked twice is one place too many for a filter to go
    /// missing in.
    ///
    /// `unread_only` is not a field here. `new/` **is** the unread box, so once
    /// [`InboxParams::mailbox`] has refused every other box beside the flag,
    /// what is left would add the predicate the box has already added — and a
    /// filter written twice is one no test can tell from one.
    pub(crate) fn inbox_query(&self, params: &InboxParams) -> Result<Query, McpError> {
        Ok(Query {
            mailbox: Some(params.mailbox()?),
            from: self.sender(params.from.as_deref())?,
            limit: Some(params.limit.unwrap_or(DEFAULT_LIMIT)),
            ..Query::default()
        })
    }

    /// The one machine a listing is restricted to, if the caller named one.
    ///
    /// Through the service, so the short id `list_peers` hands an agent works
    /// here as it does in `send` (#19). And a name that matches nothing is an
    /// error: it used to be parsed with `.ok()`, so anything that was not a
    /// whole `hm1:` fingerprint — the short form included — meant "no filter
    /// at all", and a Claude asking for one machine's mail was handed
    /// everybody's without being told (#102).
    fn sender(&self, typed: Option<&str>) -> Result<Option<NodeId>, McpError> {
        typed
            .map(|typed| {
                self.service.resolve_peer(typed.trim()).map_err(|_| {
                    McpError::invalid_params(
                        format!(
                            "`{typed}` is not a machine this node knows. `from` takes a node \
                             id, whole or in the short form `list_peers` shows."
                        ),
                        None,
                    )
                })
            })
            .transpose()
    }
}

/// How many messages a listing returns when the caller does not say.
pub(crate) const DEFAULT_LIMIT: usize = 20;

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
    ///
    /// Only `new/` holds unread mail, so `unread_only` with any other box asks
    /// for mail that is read and unread at once, and the answer was an empty
    /// list — which reads as an empty box. Refused, as `hivemind inbox
    /// --unread --box sent` has been since #79.
    fn mailbox(&self) -> Result<Mailbox, McpError> {
        let Some(raw) = self.r#box.as_deref() else {
            return Ok(Mailbox::New);
        };
        let chosen = Mailbox::from_str_opt(raw).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "`{raw}` is not a box. There are four: new (arrived, unread), \
                     cur (arrived, read), out (sent from here, not delivered yet) \
                     and sent (delivered to everyone)."
                ),
                None,
            )
        })?;

        if self.unread_only == Some(true) && chosen != Mailbox::New {
            return Err(McpError::invalid_params(
                format!("nothing in `{raw}` is unread — drop `unread_only`, or drop `box`"),
                None,
            ));
        }
        Ok(chosen)
    }
}

/// Arguments for a tool that takes only a message id.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MessageIdParams {
    /// The message id, as shown by `inbox`.
    pub id: String,
}

/// Arguments for `thread`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ThreadParams {
    /// The id of **any** message in the conversation, not only the first one:
    /// whatever `inbox` or `read` handed you, whole or shortened.
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
    /// An identical message this node sent in the last two minutes, if there
    /// was one. This message was queued regardless; #33 is an agent or a person
    /// running the same send twice, and an agent that has just been told so can
    /// decide whether it meant to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
}

impl From<Queued> for Sent {
    fn from(queued: Queued) -> Self {
        Self {
            id: queued.message.id.to_string(),
            thread_id: queued.message.thread_id.to_string(),
            duplicate_of: queued.duplicate_of.map(|id| id.to_string()),
        }
    }
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
    /// How many **other** messages are in this thread. When it is not zero,
    /// `thread` with this id returns the whole conversation in order.
    pub others_in_thread: usize,
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
                       recipient. `cur` is mail that arrived here and has been read. \
                       `from` narrows the list to one machine: pass the id `list_peers` \
                       gives you, short or whole."
    )]
    async fn inbox(
        &self,
        Parameters(params): Parameters<InboxParams>,
    ) -> Result<Json<Vec<InboxItem>>, McpError> {
        let query = self.inbox_query(&params)?;
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
                       `inbox`. `others_in_thread` says how much more was said on the \
                       same subject; `thread` with this id returns all of it. The reply \
                       you send back should go through `reply` so it stays in the same \
                       thread."
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

        // What else was said on this subject. A message answered on its own is
        // how a Claude replies to the middle of a conversation it has not read
        // (#34).
        let others = self
            .service
            .thread(message.thread_id)
            .map_err(|e| mcp_error(&e))?
            .len()
            .saturating_sub(1);

        Ok(Json(self.full(message, others)))
    }

    #[tool(
        name = "thread",
        description = "Read a whole conversation, oldest first, and mark it read. Pass the \
                       id of ANY message in the thread — whatever `inbox` or `read` handed \
                       you, short or whole — not only the first one, which nobody knows by \
                       heart. Call this when you are picking a session back up, or before \
                       answering something that has been going on for a while: `read` gives \
                       you one message, this gives you the exchange it belongs to, every \
                       body in full, with who sent each one and when. `sender_kind` is \
                       `human` where a person typed it and `agent` where another Claude \
                       did, so you can see which turns were yours. To continue the \
                       conversation, `reply` to the last id in it."
    )]
    async fn thread(
        &self,
        Parameters(params): Parameters<ThreadParams>,
    ) -> Result<Json<Vec<FullMessage>>, McpError> {
        let id = self.resolve(&params.id)?;
        let summaries = self.service.thread_of(id).map_err(|e| mcp_error(&e))?;
        let others = summaries.len().saturating_sub(1);

        let mut messages = Vec::with_capacity(summaries.len());
        for summary in &summaries {
            let (_, message) = self.service.get(summary.id).map_err(|e| mcp_error(&e))?;
            // Reading the conversation is reading the messages in it, which is
            // what the web UI's thread view already decided (SPEC §11). Only
            // what arrived and is unread: our own sent mail is in neither box
            // `mark_read` moves anything between.
            if summary.mailbox.is_unread() {
                self.service
                    .mark_read(summary.id)
                    .map_err(|e| mcp_error(&e))?;
            }
            messages.push(self.full(message, others));
        }

        Ok(Json(messages))
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
        let queued = self
            .service
            .send(draft, SenderKind::Agent)
            .map_err(|e| mcp_error(&e))?;

        Ok(Json(queued.into()))
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
        let queued = self
            .service
            .reply(
                id,
                params.body,
                local_paths(params.attachments),
                SenderKind::Agent,
            )
            .map_err(|e| mcp_error(&e))?;

        Ok(Json(queued.into()))
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

        let queued = self
            .service
            .send(draft, SenderKind::Agent)
            .map_err(|e| mcp_error(&e))?;

        Ok(Json(queued.into()))
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
    use crate::server::tests::{deliver, member, server};

    /// The subjects a listing came back with, newest first.
    async fn subjects(server: &HivemindMcp, params: InboxParams) -> Vec<String> {
        server
            .inbox(Parameters(params))
            .await
            .expect("a listing")
            .0
            .into_iter()
            .map(|item| item.subject)
            .collect()
    }

    /// The error a listing was refused with.
    ///
    /// Panics naming what came back instead, because "refused" and "filtered to
    /// nothing" are the two answers this whole file is about telling apart.
    async fn refusal(server: &HivemindMcp, params: InboxParams, asked: &str) -> McpError {
        match server.inbox(Parameters(params)).await {
            Err(error) => error,
            Ok(listed) => panic!(
                "{asked} should be refused, and it answered with {:?}",
                listed.0
            ),
        }
    }

    /// Two messages from one machine, the older of them read.
    ///
    /// Every test below starts from a box with something in it that the filter
    /// under test has to leave out. An empty list proves nothing: it is what
    /// `an_unknown_mailbox_filter_returns_nothing` asserted while the code
    /// returned everything (#28).
    fn one_read_one_unread(server: &HivemindMcp) -> hivemind_core::identity::Identity {
        let friend = member(server, 91, "ana-mbp");
        let read = deliver(server, &friend, 100, "already read");
        deliver(server, &friend, 200, "still waiting");
        server.service.mark_read(read).expect("mark read");
        friend
    }

    #[tokio::test]
    async fn the_inbox_lists_the_box_it_was_asked_for_and_no_other() {
        let (_dir, server) = server();
        one_read_one_unread(&server);

        assert_eq!(
            subjects(&server, InboxParams::default()).await,
            ["still waiting"],
            "`new` is the default, and it is the unread half"
        );
        assert_eq!(
            subjects(
                &server,
                InboxParams {
                    r#box: Some("cur".to_owned()),
                    ..InboxParams::default()
                }
            )
            .await,
            ["already read"],
            "and `cur` is the other half, not both halves"
        );
    }

    #[tokio::test]
    async fn the_inbox_can_be_narrowed_to_one_machine_by_the_id_list_peers_shows() {
        // A Claude reading `list_peers` is handed short ids. `from` used to
        // parse with `.ok()`, so a short one became no filter at all and the
        // answer was everybody's mail — the narrow question, the broad answer
        // (#102, and #19 for the same mistake in `send`).
        let (_dir, server) = server();
        let ana = member(&server, 92, "ana-mbp");
        let beto = member(&server, 93, "beto-air");
        deliver(&server, &ana, 100, "from ana");
        deliver(&server, &beto, 200, "from beto");

        let only_ana = InboxParams {
            from: Some(ana.node_id().short()),
            ..InboxParams::default()
        };
        assert_eq!(
            subjects(&server, only_ana).await,
            ["from ana"],
            "one machine's mail, not the whole box"
        );
    }

    #[tokio::test]
    async fn a_from_that_names_no_machine_is_refused_rather_than_ignored() {
        let (_dir, server) = server();
        one_read_one_unread(&server);

        let error = refusal(
            &server,
            InboxParams {
                from: Some("nobody-here".to_owned()),
                ..InboxParams::default()
            },
            "a sender this node has never met",
        )
        .await;

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("nobody-here"), "{}", error.message);
    }

    #[tokio::test]
    async fn the_inbox_stops_at_the_limit_it_was_given() {
        let (_dir, server) = server();
        let ana = member(&server, 94, "ana-mbp");
        for (millis, subject) in [(100, "oldest"), (200, "middle"), (300, "newest")] {
            deliver(&server, &ana, millis, subject);
        }

        assert_eq!(
            subjects(
                &server,
                InboxParams {
                    limit: Some(2),
                    ..InboxParams::default()
                }
            )
            .await,
            ["newest", "middle"],
            "two of the three, newest first"
        );
    }

    #[tokio::test]
    async fn unread_only_in_a_box_that_holds_no_unread_mail_is_refused() {
        // Only `new/` holds unread mail, so this asks for mail that is read and
        // unread at once. It answered with an empty list, which reads as an
        // empty box — #28 exactly. The CLI has refused it since #79.
        let (_dir, server) = server();
        one_read_one_unread(&server);

        let error = refusal(
            &server,
            InboxParams {
                r#box: Some("cur".to_owned()),
                unread_only: Some(true),
                ..InboxParams::default()
            },
            "mail that is read and unread at once",
        )
        .await;

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            error.message.contains("unread_only") && error.message.contains("box"),
            "it should say which one to drop: {}",
            error.message
        );
    }

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
    fn a_machine_this_node_does_not_know_is_the_callers_fault_too() {
        // An agent handed "there is no such machine" can look at `list_peers`
        // and try another. An agent handed "internal error" can only give up.
        let error = mcp_error(&ServiceError::NoSuchPeer {
            id: "ana-mbp".to_owned(),
        });
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("ana-mbp"), "{}", error.message);
    }

    #[test]
    fn a_broken_index_is_not_blamed_on_the_caller() {
        let error = mcp_error(&ServiceError::Unavailable);
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }
}
