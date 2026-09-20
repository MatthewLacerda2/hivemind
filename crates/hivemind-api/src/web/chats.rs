//! The conversation list page (SPEC §11, #43).
//!
//! A conversation **is** a thread, so this is the list of what `/thread/{id}`
//! opens one of. The inbox next door shows the same mail as loose messages in
//! arrival order, which for two subjects with one machine is two conversations
//! read as nine mixed lines.
//!
//! Its own file because `web.rs` is at the size the gate allows, and this is a
//! whole view rather than a line on one.

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::response::Response;

use super::{Chrome, page, render_error};
use crate::service::MailService;

/// One machine in a conversation, in both the forms a page needs: the whole id
/// for the tooltip, the short one for reading.
struct Who {
    id: String,
    short: String,
}

/// One conversation, as the page shows it.
struct ChatRow {
    thread_id: String,
    subject: String,
    participants: Vec<Who>,
    messages: u64,
    unread: u64,
    last_sender_kind: String,
    last_at: String,
    when: String,
}

#[derive(Template)]
#[template(path = "chats.html")]
struct ChatsPage {
    here: &'static str,
    unread: u64,
    node_id: String,
    short_id: String,
    chats: Vec<ChatRow>,
}

/// The conversations, the one that moved last first.
pub(super) async fn chats(State(service): State<Arc<MailService>>) -> Result<Response, Response> {
    let chrome = Chrome::new(&service, "chats");

    // One page of them, read in one go, as the listings next door are: this
    // renders a list somebody scrolls, not an API a client walks.
    let chats = service
        .conversations(&hivemind_core::index::ConversationQuery {
            with: None,
            limit: Some(200),
        })
        .map_err(render_error)?
        .into_iter()
        .map(ChatRow::from)
        .collect();

    Ok(page(ChatsPage {
        here: chrome.here,
        unread: chrome.unread,
        node_id: chrome.node_id,
        short_id: chrome.short_id,
        chats,
    }))
}

impl From<hivemind_core::index::Conversation> for ChatRow {
    fn from(chat: hivemind_core::index::Conversation) -> Self {
        Self {
            thread_id: chat.thread_id.to_string(),
            subject: chat.subject,
            participants: chat
                .participants
                .iter()
                .map(|id| Who {
                    id: id.to_string(),
                    short: id.short(),
                })
                .collect(),
            messages: chat.messages,
            unread: chat.unread,
            last_sender_kind: chat.last_sender_kind.as_str().to_owned(),
            last_at: chat.last_at.to_rfc3339(),
            when: super::relative(chat.last_at),
        }
    }
}

#[cfg(test)]
mod tests {
    use hivemind_core::message::{Kind, Recipient, SenderKind};

    use crate::service::Draft;
    use crate::web::page_tests::{app, get};

    #[tokio::test]
    async fn the_page_lists_one_conversation_per_subject_and_links_to_each() {
        // Two conversations with one machine, which is #43: the inbox shows
        // these three messages as three rows, and which belong together is
        // left to whoever is reading.
        let (_dir, router, service) = app();
        let opened = service
            .send(
                Draft {
                    to: vec![Recipient::Node(service.identity())],
                    subject: "dashboard PR".to_owned(),
                    body: "take a look".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Human,
            )
            .expect("send")
            .message;
        service
            .reply(opened.id, "on it".to_owned(), Vec::new(), SenderKind::Agent)
            .expect("reply");
        service
            .send(
                Draft {
                    to: vec![Recipient::Node(service.identity())],
                    subject: "lunch?".to_owned(),
                    body: "1pm?".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Human,
            )
            .expect("send");

        let page = get(&router, "/chats").await;

        page.says("dashboard PR")
            .says("lunch?")
            // The subject each opened with, not the `Re:` its last message
            // carries.
            .says(&format!("/thread/{}", opened.thread_id))
            // Counted once each: a message addressed to its own sender is in
            // two boxes, and the index holds a row for each (#34).
            .says("2 messages")
            .says("1 message")
            .says("2 unread")
            // The badge is the last message's, and a Claude wrote that one.
            .says("badge agent")
            // A conversation is shown by the subject it opened with.
            .does_not_say("Re: dashboard PR");
    }

    #[tokio::test]
    async fn an_empty_machine_says_so_rather_than_showing_an_empty_list() {
        let (_dir, router, _service) = app();
        get(&router, "/chats").await.says("Nothing here yet");
    }
}
