//! Reading what is here: listings, one message, a conversation, read state.
//!
//! Everything in here answers from the index and the mail files and changes
//! nothing, apart from `mark_read` and `reindex`, which are the two ways a
//! reader alters what a later read says.

use super::*;

impl MailService {
    /// Run a listing query.
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn list(&self, query: &Query) -> Result<Vec<Summary>, ServiceError> {
        Ok(self.index()?.search(query)?)
    }

    /// Fetch one message, wherever it is.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if no mailbox holds it.
    pub fn get(&self, id: Ulid) -> Result<(Mailbox, Message), ServiceError> {
        // Prefer the received copy: a message addressed to its own sender
        // exists twice, and "read this" means the one in the inbox.
        for mailbox in [Mailbox::New, Mailbox::Cur, Mailbox::Sent, Mailbox::Out] {
            match self.store.get(mailbox, id) {
                Ok(message) => return Ok((mailbox, message)),
                Err(StoreError::NotFound { .. }) => {}
                Err(other) => return Err(other.into()),
            }
        }
        Err(ServiceError::NoSuchMessage { id })
    }

    /// Every message in a thread, oldest first, once each.
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn thread(&self, thread_id: Ulid) -> Result<Vec<Summary>, ServiceError> {
        let mut found = self.index()?.search(&Query {
            thread: Some(thread_id),
            ..Query::default()
        })?;
        // A conversation reads forwards.
        found.reverse();

        // The index holds a row per id *and* mailbox, so a message addressed to
        // its own sender is in it twice: once as what arrived, once as what was
        // sent. A listing should show both, because both boxes hold it. A
        // conversation showing both reads as the same thing having been said
        // twice, so the arrived copy wins, as it does in `get`.
        let arrived: std::collections::HashSet<Ulid> = found
            .iter()
            .filter(|summary| matches!(summary.mailbox, Mailbox::New | Mailbox::Cur))
            .map(|summary| summary.id)
            .collect();
        found.retain(|summary| {
            matches!(summary.mailbox, Mailbox::New | Mailbox::Cur) || !arrived.contains(&summary.id)
        });
        Ok(found)
    }

    /// Every message in the conversation `id` belongs to, oldest first.
    ///
    /// The id of **any** message in the thread, not only its root: nobody knows
    /// by heart which one was first, and the id somebody has to hand is the one
    /// they were just reading (#34).
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if no mailbox holds `id`. A conversation
    /// nobody has is absent rather than empty, and an empty list would read as
    /// the second (#28).
    pub fn thread_of(&self, id: Ulid) -> Result<Vec<Summary>, ServiceError> {
        self.thread(self.thread_id_of(id)?)
    }

    /// Turn what somebody typed into a message id (SPEC §10).
    ///
    /// Accepts the whole ULID, or **any tail of one that names exactly one
    /// message** — which is what makes the short id the inbox prints usable.
    /// It was not: `hivemind inbox` showed `03VYRM`, `hivemind read 03VYRM`
    /// answered "not a message id", and only the 26-character form worked,
    /// which the CLI never showed anywhere (#27). A short form a program
    /// prints and then refuses is not informing anybody; it is misleading
    /// them, and copying what is on the screen is the obvious gesture.
    ///
    /// The same shape `resolve_peer` has had since #19, for the same reason.
    ///
    /// An ambiguous tail is an **error naming the candidates**, never a
    /// guess: reading the wrong message is worse than being asked again.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessageTail`] if nothing matches,
    /// [`ServiceError::AmbiguousMessage`] if more than one does, or
    /// [`ServiceError::Unavailable`] if the index lock is poisoned.
    pub fn resolve_message(&self, typed: &str) -> Result<Ulid, ServiceError> {
        let typed = typed.trim();
        if let Ok(id) = typed.parse::<Ulid>() {
            return Ok(id);
        }

        // Two is all the caller needs: "one" or "more than one". Fetching the
        // rest to count them would be work nobody reads.
        let found = self
            .index
            .lock()
            .map_err(|_| ServiceError::Unavailable)?
            .ids_ending_with(typed, 2)?;

        match found.as_slice() {
            [id] => Ok(*id),
            [] => Err(ServiceError::NoSuchMessageTail {
                typed: typed.to_owned(),
            }),
            _ => Err(ServiceError::AmbiguousMessage {
                typed: typed.to_owned(),
                candidates: found,
            }),
        }
    }

    /// Move a message from `new` to `cur` (SPEC §7.1).
    ///
    /// Marking an already-read message read again is not an error: it is what
    /// a second click does.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if it is in neither mailbox.
    pub fn mark_read(&self, id: Ulid) -> Result<(), ServiceError> {
        match self.store.move_to(Mailbox::New, Mailbox::Cur, id) {
            Ok(()) => {
                self.index()?.set_mailbox(id, Mailbox::New, Mailbox::Cur)?;
                let _ = self.events.send(Event::MessageRead { id });
                Ok(())
            }
            Err(StoreError::NotFound { .. }) => {
                // Already read is success; never received is not.
                match self.store.get(Mailbox::Cur, id) {
                    Ok(_) => Ok(()),
                    Err(_) => Err(ServiceError::NoSuchMessage { id }),
                }
            }
            Err(other) => Err(other.into()),
        }
    }

    /// How many unread messages there are (SPEC §9.3).
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn unread_count(&self) -> Result<u64, ServiceError> {
        Ok(self.index()?.unread_count()?)
    }

    /// Throw the index away and rebuild it from the mail files (SPEC §10).
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn reindex(&self) -> Result<(), ServiceError> {
        self.index()?.rebuild_from(&self.store)?;
        Ok(())
    }
    pub(super) fn thread_id_of(&self, parent: Ulid) -> Result<Ulid, ServiceError> {
        let (_, message) = self.get(parent)?;
        Ok(message.thread_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::{draft_to_self, service};

    #[test]
    fn marking_a_message_read_moves_it_out_of_the_unread_count() {
        let (_dir, service) = service();
        let sent = service
            .send(draft_to_self(&service, "unread", "body"), SenderKind::Human)
            .expect("send")
            .message;
        assert_eq!(service.unread_count().expect("count"), 1);

        service.mark_read(sent.id).expect("mark read");
        assert_eq!(service.unread_count().expect("count"), 0);
        assert_eq!(service.get(sent.id).expect("get").0, Mailbox::Cur);
    }

    #[test]
    fn marking_an_already_read_message_read_again_is_not_an_error() {
        let (_dir, service) = service();
        let sent = service
            .send(draft_to_self(&service, "unread", "body"), SenderKind::Human)
            .expect("send")
            .message;
        service.mark_read(sent.id).expect("first");
        service
            .mark_read(sent.id)
            .expect("a second click is not a failure");
    }

    #[test]
    fn marking_a_message_we_never_received_read_is_an_error() {
        let (_dir, service) = service();
        assert!(matches!(
            service.mark_read(Ulid::generate()),
            Err(ServiceError::NoSuchMessage { .. })
        ));
    }
    #[test]
    fn a_thread_reads_oldest_first() {
        let (_dir, service) = service();
        let root = service
            .send(draft_to_self(&service, "lunch", "?"), SenderKind::Human)
            .expect("send")
            .message;
        service
            .reply(root.id, "yes".to_owned(), Vec::new(), SenderKind::Human)
            .expect("reply");

        let thread = service.thread(root.thread_id).expect("thread");
        assert!(thread.len() >= 2);
        assert!(
            thread.windows(2).all(|w| w[0].sent_at <= w[1].sent_at),
            "a conversation reads forwards"
        );
    }
}
